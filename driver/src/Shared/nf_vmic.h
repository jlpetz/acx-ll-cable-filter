/*++

    noise-filter (TODO 19, spike checks 4 and 5): the protocol between the app and the virtual microphone driver.

    Original to noise-filter, not from Microsoft's sample: MIT OR Apache-2.0, like the Rust crate that mirrors it
    (crates/nf-vmic/src/protocol.rs, which checks these offsets at compile time). Change the two together. Bump
    NF_VMIC_VERSION on a change to a layout or to a rule the app must follow (ATTACH and its outcomes, the ring's header, and
    the ring's rules for the app's side: its positions, WriteFrame and ReadFrame, the fill, the State bits). NF_VMIC_STATS
    also carries its size (StatsBytes), which a reader checks with the version, so it has grown without one; and the driver's
    handling of its other capture streams (the reader's copy, handoffs, their counters) can change without one, as it has
    at version 1 (the owner, 2026-11-23). Ordinary ATTACH, its header and STATS keep version 1. Live ATTACH and its header
    negotiate version 2 with NF_VMIC_ATTACH_LIVE; older drivers refuse that request with STATUS_REVISION_MISMATCH.

Module Name:

    nf_vmic.h

Abstract:

    One named control device, \Device\NoiseFilterVmic (symbolic link \DosDevices\NoiseFilterVmic), opened from user mode as
    \\.\NoiseFilterVmic with FILE_FLAG_OVERLAPPED, takes two IOCTLs:

    NF_VMIC_IOCTL_ATTACH (METHOD_OUT_DIRECT, read and write access). Input: an NF_VMIC_ATTACH_IN. Output: the app's ring,
        exactly NF_VMIC_RING_BYTES(capacity) long and starting on a page boundary (VirtualAlloc's memory does): an
        NF_VMIC_RING_HEADER, then the frames. The I/O manager probes and locks the pages for the request's life; the driver maps
        them no-execute and keeps the request pending, reading the ring from the capture stream's timer, until the request ends.
        One attachment at a time.

        Refusals complete the request without publishing the ring. KMDF pends every request it delivers to the driver, so on
        the overlapped handle each refusal below returns ERROR_IO_PENDING from DeviceIoControl, and then its status from
        GetOverlappedResult. A failure DeviceIoControl returns at once comes from before the driver saw the request: the I/O
        manager's access check (ERROR_ACCESS_DENIED, 5: a handle opened without read and write access), its capture of the
        input or its probe and lock of the output buffer for writing (ERROR_NOACCESS, 998, for a buffer it can't read or write;
        other codes, ERROR_NO_SYSTEM_RESOURCES 1450 among them, if it can't allocate or lock), or a purged queue (ERROR_BAD_COMMAND,
        below). 998 and 1450 are in the list below too: which call returned them tells them apart. The driver checks the input's
        Magic and Version before any size, so a client of another version always gets STATUS_REVISION_MISMATCH:
          - STATUS_INVALID_DEVICE_REQUEST (ERROR_INVALID_FUNCTION, 1): not from user mode.
          - STATUS_REVISION_MISMATCH (ERROR_REVISION_MISMATCH, 1306): unsupported input version, or a header version different from the input.
          - STATUS_INVALID_BUFFER_SIZE (ERROR_INVALID_USER_BUFFER, 1784): the output buffer is shorter than NF_VMIC_MIN_RING_BYTES
            or longer than NF_VMIC_MAX_RING_BYTES, or its length isn't the header's TotalBytes and NF_VMIC_RING_BYTES(CapacityFrames).
          - STATUS_INVALID_PARAMETER (ERROR_INVALID_PARAMETER, 87): anything else in the input or the header, or an output buffer
            that isn't one MDL of exactly the buffer.
          - STATUS_DATATYPE_MISALIGNMENT (ERROR_NOACCESS, 998, from GetOverlappedResult): the output buffer doesn't start on a
            page boundary.
          - STATUS_INSUFFICIENT_RESOURCES (ERROR_NO_SYSTEM_RESOURCES, 1450, from GetOverlappedResult): the driver couldn't map
            the ring into system space.
          - STATUS_DEVICE_REMOVED (ERROR_DEVICE_REMOVED, 1617): the device is going away.
          - STATUS_DEVICE_BUSY (ERROR_BUSY, 170): another ring is attached.
          - STATUS_NOT_SUPPORTED (ERROR_NOT_SUPPORTED, 50): a capture stream is open (created and not yet destroyed, running
            or not) in a format other than 48 kHz float32 stereo: in practice an exclusive-mode client's, since shared-mode
            streams run at the endpoint's device format. The driver leaves that client alone. The app should feed through its
            WASAPI output instead (24-in-32 into the speaker endpoint, which the loopback carries to the microphone), or
            ATTACH again once that stream has closed.
          - STATUS_CANCELLED (ERROR_OPERATION_ABORTED, 995): the request was cancelled, or the handle closed, before the ring was
            published.

        Otherwise the ring is published: the header's State turns NF_VMIC_STATE_ATTACHED while the request stays pending (the
        app's sign that the ring is live). It ends with one of:
          - STATUS_CANCELLED (995): CancelIoEx, the thread that issued it or its process exiting, or the handle closed.
          - STATUS_DEVICE_REMOVED (1617): the device is going away (its last PnP device removed, disabled or updated). Close
            every handle to the control device then: the framework destroys a deleted control device only once the last handle
            closes, and until then it keeps the driver loaded. The driver deletes the symbolic link with the device, so the name
            is free at once, and a PnP device that comes back gets a new control device under it. A handle opened earlier
            stays on the deleted device, where every request fails at once (DeviceIoControl returns ERROR_BAD_COMMAND, 22:
            STATUS_INVALID_DEVICE_STATE, the queue is purged): open a new one. With no control device the open fails with
            ERROR_FILE_NOT_FOUND (2).
        Either way the driver unpublishes the ring before it completes the request, and never touches the ring after.

    NF_VMIC_IOCTL_STATS (METHOD_BUFFERED, read access). No input. Output: an NF_VMIC_STATS.

    Include it after the platform headers (kernel: ntddk.h; user mode: windows.h and winioctl.h), which give CTL_CODE, the
    UINT8-UINT64 types, C_ASSERT and FIELD_OFFSET.

The ring's rules:

    - Positions are absolute 64-bit frame counts from 0, never wrapped indexes. Frame n is in slot n & (CapacityFrames - 1),
      at byte NF_VMIC_HEADER_BYTES + slot * NF_VMIC_FRAME_BYTES; a frame is two IEEE 754 binary32 samples, left then right.
    - Before ATTACH the app zeroes the header, fills in line 0, and sets WriteFrame to the frames it has already written (0, or
      a pre-fill of up to CapacityFrames frames from position 0). The driver starts reading at frame 0.
    - The app writes frames at WriteFrame and after, then stores WriteFrame with release ordering. It may write only slots in
      [WriteFrame, ReadFrame + CapacityFrames): the free space is CapacityFrames - (WriteFrame - ReadFrame).
    - At each capture pass (P frames: the capture stream's packet) the driver reads WriteFrame with acquire ordering. With
      fill = WriteFrame - ReadFrame as an unsigned 64-bit difference: if fill > CapacityFrames (WriteFrame went backwards, or
      ran more than a ring ahead) it re-syncs, ReadFrame = WriteFrame, so the ring reads as empty, and counts it. It then copies
      min(fill, P) frames from ReadFrame, zero-fills the rest of the packet (an underrun, counted), advances ReadFrame by the
      frames copied, and stores ReadFrame with release ordering after the copy. With ordinary ATTACH every frame the app writes
      is delivered once and in order; an underrun inserts silence and drops nothing; a valid fill never causes a re-sync.
    - NF_VMIC_ATTACH_LIVE discards unread audio on the first pass after all capture readers have stopped, including first open.
      A live microphone producer skips writes while idle. During LIVE_HANDOFF another reader is still running: keep feeding at
      the previous packet size until it takes the slot. Ordinary attachments (Flags = 0) retain their queued frames across idle.
    - The capacity, and the fill the app keeps, must be at least one capture packet (PacketFrames: 480 frames for a shared
      stream's 10 ms period), or every pass underruns. NF_VMIC_MIN_CAPACITY_FRAMES holds two such packets.
    - One capture stream holds the reader slot at a time: the first to run after ATTACH (or after the last one began to pause:
      a stream gives the slot up as its Pause starts, while its timer still runs, and may hold it again only after its next
      Run). Only the reader reads the ring and moves ReadFrame, so its passes set the pace. The driver keeps a copy of the
      packets it delivers (its underrun silence included) on the stream engine's QPC time line, and every other capture stream
      running in 48 kHz float32 stereo plays that copy a lag behind: the reader's packet plus 1 ms and a frame. More than one
      capture stream can run at once: the microphones of two of the driver's devices share the one app ring, and the audio
      engine opens a device stream per processing mode (a RAW and a DEFAULT client make two). The other streams' passes are
      counted in NF_VMIC_STATS only (NotReaderPackets, SecondaryFramesCopied, SecondaryUnderrunFrames, SecondarySkips,
      SecondarySkippedFrames).
    - No capture stream gets any of the ring's frames twice. The copy holds the ring's frames in the order they were
      delivered, none left out at a handoff: a new reader whose packet starts before the copy's end takes that part from the
      copy, and reads the ring on from there. A stream playing the copy keeps its place in it and never goes back: where its
      place would move back (a handoff to a reader with a longer packet; a Pause and Run soon after it last played the copy or
      held the slot) it gets silence until it passes what it has had, counted in SecondaryUnderrunFrames. A handoff (the
      reader pauses and another stream takes the slot) skips once in the stream that takes it: its first packet as the reader
      starts at its own place on the time line, a lag ahead of its place in the copy, so what the copy held between the two
      never reaches it (its lag at most: 209 frames behind 160-frame readers), counted in ReaderHandoffs and
      HandoffSkippedFrames (the owner, 2026-11-20: a one-time skip for now; a gapless handoff is a follow-up). Any other
      stream plays on through a handoff, repeating nothing: it gets silence where the new reader's packet is longer (its place
      moves back by the difference) or the readers' time lines leave a gap; where it is shorter, it moves on to the new
      reader's lag at once and skips the difference once (320 frames from a 480-frame reader to a 160-frame one), counted in
      SecondarySkips and SecondarySkippedFrames, apart from the taker's skip, so each stream's gap has its own count (the
      owner, 2026-11-23: lower latency over a one-time skip; from a66d8ab to e801320 it kept the longer lag until it next
      ran, after Astra's review had found such a stream moving back and playing the difference again, 320 frames from a
      160-frame reader to a 480-frame one). A stream that is pausing (it gave the slot up and its timer is stopping) keeps
      its lag. A jump in a stream's time line (a late timer's glitch adjustment,
      Stream[NF_VMIC_KIND_CAPTURE].GlitchPasses) moves the copy's audio by its length: the reader's inserts that much silence
      in the other streams (its own output doesn't show it), another stream's skips that much of the copy. So does what the
      copy didn't hold in time for a stream (the reader's timer later than its own by more than the 1 ms): silence there,
      counted in SecondaryUnderrunFrames, and those frames never reach it. A reader that kept the slot while its timer stopped
      did the same to the streams that played on (55 to 111 frames lost at live handoffs, 2026-11-23): it now gives the slot
      up first.
    - While a ring is attached, creating a capture stream in any format other than 48 kHz float32 stereo fails with
      STATUS_NOT_SUPPORTED (counted in RefusedCaptureOpens; an exclusive-mode client can retry in float32). An ATTACH while
      such a stream is open fails the same way (counted in FormatRejects; the refusals above). The check and the publish (or
      the record) are one locked step, so the two can't both succeed.
    - Frames are copied only while the stream holding the slot runs 48 kHz float32 stereo (the endpoint's device format, spike
      check 3). The refusals above leave this as a fallback, for a stream whose packets don't hold whole frames: its packets
      are zero-filled and counted (MismatchPackets), State reads FORMAT_MISMATCH instead of READING, and ReadFrame stays
      where it is. Passes and LastPassQpc count the slot holder's passes either way: a rising Passes says a stream holds the
      slot, not that it reads. FramesCopied measures reading.
    - The driver copies the header once at ATTACH, checks the copy and keeps what it needs from it; it never reads line 0 or
      its own line back. Of the app's memory it reads only WriteFrame (every pass, untrusted) and the frames. It writes the
      header only while the ring is published, under its lock.

NF_VMIC_RING_HEADER (256 bytes, four 64-byte lines; little-endian; the frames start at byte 256):

    Offset Size Field            Writer  Rule
    ------ ---- ---------------- ------- -------------------------------------------------------------------------------
    Line 0 (0-63): set by the app before ATTACH; the driver copies it once and checks it.
       0     4  Magic            app     NF_VMIC_MAGIC
       4     4  Version          app     the request's Version (NF_VMIC_VERSION or NF_VMIC_LIVE_VERSION)
       8     4  HeaderBytes      app     NF_VMIC_HEADER_BYTES
      12     4  CapacityFrames   app     a power of two, NF_VMIC_MIN_CAPACITY_FRAMES to NF_VMIC_MAX_CAPACITY_FRAMES
      16     4  SampleRate       app     NF_VMIC_SAMPLE_RATE (48000)
      20     2  Channels         app     NF_VMIC_CHANNELS (2)
      22     2  SampleFormat     app     NF_VMIC_FORMAT_FLOAT32
      24     4  FrameBytes       app     NF_VMIC_FRAME_BYTES (8)
      28     4  Flags            app     0
      32     8  TotalBytes       app     NF_VMIC_RING_BYTES(CapacityFrames): the output buffer's length, exactly
      40    24  Reserved0        app     0
    Line 1 (64-127): the app's.
      64     8  WriteFrame       app     frames written; stored with release ordering after the frames
      72    56  Reserved1        app     0
    Line 2 (128-191): the driver's, zeroed at ATTACH; the app only reads it.
     128     8  ReadFrame        driver  the next frame the driver reads; stored with release ordering after the copy
     136     8  FramesCopied     driver  this attachment's frames copied into capture packets
     144     8  UnderrunFrames   driver  frames zero-filled because the ring didn't hold them yet
     152     4  Resyncs          driver  re-syncs (above)
     156     4  MismatchPackets  driver  packets zero-filled because the capture stream wasn't 48 kHz float32 stereo
     160     4  State            driver  NF_VMIC_STATE_* bits (0 once detached)
     164     4  PacketFrames     driver  the packet of the stream holding the reader slot, in frames (0: none; see STATS)
     168     8  Passes           driver  capture passes of the stream holding the reader slot, FORMAT_MISMATCH ones included
     176     8  LastPassQpc      driver  KeQueryPerformanceCounter at the latest of those passes
     184     8  ClockSequence    driver  live attachments: even for stable ReadFrame, LastPassQpc and PacketFrames; odd while updating
    Line 3 (192-255): reserved, 0.
     256        frames           app     CapacityFrames x NF_VMIC_FRAME_BYTES

NF_VMIC_STATS (672 bytes; all counters cumulative since the driver loaded; times in 100 ns units):

    Offset Size Field                    Content
    ------ ---- ------------------------ -----------------------------------------------------------------------------
       0     4  Magic, 4 Version, 8 StatsBytes (672), 12 Flags (NF_VMIC_STATS_*)
      16     8  QpcFrequency             KeQueryPerformanceCounter's frequency
      24     8  QpcNow                   the QPC when the snapshot was taken
      32     4  AttachedPid              the attached app's process id (0: none)
      36     4  AttachedCapacityFrames
      40     8  AttachedReadFrame        the driver's position in the attached ring
      48     8  AttachedWriteFrame       the WriteFrame the latest capture pass read (untrusted)
      56     4  ReaderPacketFrames       the packet of the stream holding the reader slot, in frames (0: none)
      60     4  Reserved0
      64   128  Ring                     NF_VMIC_RING_STATS
     192    96  Loopback                 NF_VMIC_LOOPBACK_STATS (valid with NF_VMIC_STATS_LOOPBACK)
     288     8  SecondaryFramesCopied    frames of the ring's audio delivered from the reader's copy: to capture streams other
                                         than the reader, and to a new reader for the time the copy already held
     296     8  SecondaryUnderrunFrames  frames zero-filled in their place: the ring's audio for the position wasn't there, or
                                         the stream had had it already
     304     8  ReaderHandoffs           times a capture stream that was playing the copy took the slot another stream held
     312     8  HandoffSkippedFrames     the copy's frames such a stream never delivered: between its place in the copy and its
                                         first packet as the reader (the last reader's underrun silence there included; its
                                         recorded gaps not)
     320   256  Stream[2]                NF_VMIC_STREAM_STATS: [NF_VMIC_KIND_RENDER], [NF_VMIC_KIND_CAPTURE]
     576    64  CaptureTrace             NF_VMIC_CAPTURE_TRACE: what capture streams told ACX of their packets' times
     640     8  SecondarySkips           times a capture stream playing the copy moved on at once to a shorter reader packet's
                                         lag (it skipped the difference once; the taker's skip is HandoffSkippedFrames)
     648     8  SecondarySkippedFrames   the copy's frames those skips passed over (the last reader's underrun silence there
                                         included; its recorded gaps not)
     656     8  LoopbackLeadSkips        times a loopback capture read moved on at once to a shorter render lead (valid with
                                         NF_VMIC_STATS_LOOPBACK, as Loopback is: the first-registered device's)
     664     8  LoopbackLeadSkippedFrames
                                         the loopback ring's audio those skips passed over (the difference in leads at most)

    The reader (with NF_VMIC_STATS_ATTACHED): Flags' READING and FORMAT_MISMATCH are the State bits the stream holding the
    reader slot wrote to the header at its latest pass, and ReaderPacketFrames is the PacketFrames it wrote, so STATS and the
    header agree. While no stream holds the slot all three are 0. A stream in another format holds the slot without reading:
    FORMAT_MISMATCH, and PacketFrames in its own format's frames (0 if the driver can't count its packet in frames).

    The other capture streams (the ring's rules): SecondaryFramesCopied counts frames the reader delivered, copied into
    their packets from the reader's copy, its underrun silence included (the ring's UnderrunFrames counts that), and the
    part of a new reader's first packet the copy already held. SecondaryUnderrunFrames counts the frames of silence they got
    because no reader delivered audio for their position: no reader pass in the ring's format has reached it yet (the start
    of an attachment, a reader in another format, a reader's timer later than theirs by more than the 1 ms), the reader's
    time line jumped past it (a handoff, or a glitch adjustment: the copy holds silence there), or their lag and packet
    exceed the copy (0.68 s); and the silence they got because they had had the copy's frames there already (their place
    moved back: a handoff to a reader with a longer packet, a quick Pause and Run). That includes a pausing reader's own
    passes: a reader gives the slot up as its Pause begins and plays the copy until its timer stops, and the copy there is
    what it delivered itself, so such a pass (usually one, a packet, for a client that is stopping) is silence, counted
    here. A total over the streams, such as verify's allowance for a recording's silence, can so hide up to that much of
    another stream's uncounted silence around a handoff (ours, 2026-11-23: documented rather than left uncounted; the
    owner's to confirm). The driver remembers the latest 32 jumps, which covers a 10 ms stream beside a 2 ms reader that
    jumps at every pass (a 1 ms one in the 96-frame trial build); beyond that, the older ones' silence counts as copied. A
    stream other than the reader in another format gets silence, counted in the ring's MismatchPackets.
    Ring.NotReaderPackets counts all their passes. SecondarySkips counts the times one of them moved on to a shorter reader
    packet's lag (a handoff to a reader with a shorter packet: the owner, 2026-11-23), and SecondarySkippedFrames the copy's
    frames it so passed over, counted as HandoffSkippedFrames is: from the copy's valid start to its end, less its recorded
    gaps. So a recording that played on through a handoff to a shorter packet has a gap of exactly those frames, as long
    as the copy held the span (below: frames it didn't yet hold are lost uncounted, as for any read past its end), and one
    that took the slot a
    gap of HandoffSkippedFrames (the last reader's underrun silence in either span counts but is no gap). The copy holds the
    whole span unless the new reader's timer is later than the skipping stream's by more than the 1 ms, which leaves that
    stream's read short too (SecondaryUnderrunFrames).

    The loopback's capture side (spike check 7) takes a change in the render lead at once (the owner, 2026-11-23, for a new
    render run's lead; within a run, following the owner's app-ring decision of 2026-11-23). A fall moves the read on by the
    difference: LoopbackLeadSkips counts it, and LoopbackLeadSkippedFrames the render audio the ring held between the reads
    at the two leads (what it doesn't hold yet there, only where a client released a packet too late to go early, isn't
    counted: the shorter lead's read underruns there too, Loopback.UnderrunFrames).

    NF_VMIC_STREAM_STATS (128 bytes), per stream kind: TimerPasses (passes the stream's timer started), GlitchPasses (passes
    run at once because the next was already due: each is one glitch adjustment of the stream's time line), GlitchHns (their
    sum), LatenessMaxHns, LatenessSumHns, EarlyPasses (timer passes before their time: rounding, expected 0), Runs
    (Pause-to-Run), WorkQpcTicks, LatenessBuckets[8]. WorkQpcTicks: the QPC ticks the timer passes spent working, from the
    timer callback's first clock read to the end of StreamPassCallback (the packet's work, the notification, the next pass armed,
    and any glitch passes run at once), so WorkQpcTicks / TimerPasses / QpcFrequency is the mean CPU time of a pass. A timer pass's lateness is the time it ran (its QPC, in 100 ns units)
    minus the time ScheduleNextPass armed the timer for. Buckets: below 50 us, 50-100, 100-250, 250-500 us, 0.5-1 ms, 1-2 ms,
    2-5 ms, and 5 ms and over (NF_VMIC_LATENESS_BOUNDS_HNS are the upper bounds of the first seven).

    NF_VMIC_CAPTURE_TRACE (64 bytes; TODO 19, the capture-timestamp trace): PacketQueries counts EvtAcxStreamGetCapturePacket
    calls, and LastQueryQpc, LastQueryPacket and LastQueryPacketQpc are the latest one's clock reading and answer (the packet
    number and its QPCPacketStart). LastNotifyPacket and LastNotifyQpc are the latest AcxRtStreamNotifyPacketComplete's
    packet and QPCCompletion. PositionQueries and CurrentPacketQueries count EvtAcxStreamGetPresentationPosition and
    EvtAcxStreamGetCurrentPacket calls. All capture streams', the latest of any of them; QPCs in ticks; each field is written
    whole, the set not as one.

--*/

#ifndef NF_VMIC_H
#define NF_VMIC_H
#pragma once

#define NF_VMIC_MAGIC                   0x4D56464Eu     // "NFVM" in memory order
#define NF_VMIC_VERSION                 1u
#define NF_VMIC_LIVE_VERSION            2u              // opt-in ATTACH and ring header only; ordinary ATTACH and STATS stay v1

#define NF_VMIC_DEVICE_NAME             L"\\Device\\NoiseFilterVmic"
#define NF_VMIC_SYMLINK_NAME            L"\\DosDevices\\NoiseFilterVmic"
#define NF_VMIC_USER_PATH               L"\\\\.\\NoiseFilterVmic"

// 0x0022E002 and 0x00226004.
#define NF_VMIC_IOCTL_ATTACH            CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_OUT_DIRECT, FILE_READ_ACCESS | FILE_WRITE_ACCESS)
#define NF_VMIC_IOCTL_STATS             CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_ACCESS)

// The ring's format: the capture endpoint's device format (spike check 3).
#define NF_VMIC_SAMPLE_RATE             48000u
#define NF_VMIC_CHANNELS                2u
#define NF_VMIC_FORMAT_FLOAT32          1u              // IEEE 754 binary32, interleaved left, right
#define NF_VMIC_FRAME_BYTES             8u

#define NF_VMIC_HEADER_BYTES            256u
#define NF_VMIC_DRIVER_LINE_OFFSET      128u            // line 2, the driver's
#define NF_VMIC_LINE_BYTES              64u
#define NF_VMIC_MIN_CAPACITY_FRAMES     1024u           // 21.3 ms: two 10 ms shared-mode packets (480 frames each)
#define NF_VMIC_MAX_CAPACITY_FRAMES     262144u         // 2^18: 5.46 s, 2 MiB
#define NF_VMIC_RING_BYTES(capacity)    ((UINT64)NF_VMIC_HEADER_BYTES + (UINT64)(capacity) * NF_VMIC_FRAME_BYTES)
#define NF_VMIC_MIN_RING_BYTES          NF_VMIC_RING_BYTES(NF_VMIC_MIN_CAPACITY_FRAMES)     // 8,448
#define NF_VMIC_MAX_RING_BYTES          NF_VMIC_RING_BYTES(NF_VMIC_MAX_CAPACITY_FRAMES)     // 2,097,408

// NF_VMIC_RING_HEADER.State bits.
// Live producer: the first pass after all readers have stopped drops pre-existing audio. Ordinary feeds preserve every frame.
#define NF_VMIC_ATTACH_LIVE             0x00000001u

#define NF_VMIC_STATE_ATTACHED          0x00000001u     // the driver published this ring
#define NF_VMIC_STATE_READING           0x00000002u     // the stream holding the reader slot read it at its latest pass (underrun or not)
#define NF_VMIC_STATE_FORMAT_MISMATCH   0x00000004u     // that stream isn't 48 kHz float32 stereo: silence, the ring not read
#define NF_VMIC_STATE_LIVE_HANDOFF      0x00000008u     // live attachment: another running reader can take the vacant slot

// NF_VMIC_STATS.Flags bits.
#define NF_VMIC_STATS_ATTACHED          0x00000001u     // an app ring is attached (the Attached* fields are valid)
#define NF_VMIC_STATS_LOOPBACK          0x00000002u     // the PnP device is there (Loopback is valid)
#define NF_VMIC_STATS_READING           0x00000004u     // the header's NF_VMIC_STATE_READING (the reader's latest pass)
#define NF_VMIC_STATS_FORMAT_MISMATCH   0x00000008u     // the header's NF_VMIC_STATE_FORMAT_MISMATCH (the reader's latest pass)

// NF_VMIC_STATS.Stream indexes.
#define NF_VMIC_KIND_RENDER             0u
#define NF_VMIC_KIND_CAPTURE            1u
#define NF_VMIC_KINDS                   2u

#define NF_VMIC_LATENESS_BUCKETS        8u
#define NF_VMIC_LATENESS_BOUNDS_HNS     { 500, 1000, 2500, 5000, 10000, 20000, 50000 }

// Magic and Version are the first 8 bytes of this and of the ring's header in every version. ATTACH checks the input's before any
// size, and the header's before its other fields.
typedef struct _NF_VMIC_ATTACH_IN {
    UINT32  Magic;                      //   0: NF_VMIC_MAGIC
    UINT32  Version;                    //   4: NF_VMIC_VERSION or NF_VMIC_LIVE_VERSION
    UINT32  InBytes;                    //   8: sizeof(NF_VMIC_ATTACH_IN)
    UINT32  Flags;                      //  12: 0 or NF_VMIC_ATTACH_LIVE
} NF_VMIC_ATTACH_IN, *PNF_VMIC_ATTACH_IN;

typedef struct _NF_VMIC_RING_HEADER {
    // Line 0: the app's, set before ATTACH.
    UINT32  Magic;                      //   0
    UINT32  Version;                    //   4
    UINT32  HeaderBytes;                //   8
    UINT32  CapacityFrames;             //  12
    UINT32  SampleRate;                 //  16
    UINT16  Channels;                   //  20
    UINT16  SampleFormat;               //  22
    UINT32  FrameBytes;                 //  24
    UINT32  Flags;                      //  28
    UINT64  TotalBytes;                 //  32
    UINT8   Reserved0[24];              //  40
    // Line 1: the app's.
    UINT64  WriteFrame;                 //  64
    UINT8   Reserved1[56];              //  72
    // Line 2: the driver's.
    UINT64  ReadFrame;                  // 128
    UINT64  FramesCopied;               // 136
    UINT64  UnderrunFrames;             // 144
    UINT32  Resyncs;                    // 152
    UINT32  MismatchPackets;            // 156
    UINT32  State;                      // 160
    UINT32  PacketFrames;               // 164
    UINT64  Passes;                     // 168
    UINT64  LastPassQpc;                // 176
    UINT64  ClockSequence;              // 184: live attachment only
    // Line 3.
    UINT8   Reserved3[64];              // 192
} NF_VMIC_RING_HEADER, *PNF_VMIC_RING_HEADER;

typedef struct _NF_VMIC_STREAM_STATS {
    UINT64  TimerPasses;                //   0
    UINT64  GlitchPasses;               //   8
    UINT64  GlitchHns;                  //  16
    UINT64  LatenessMaxHns;             //  24
    UINT64  LatenessSumHns;             //  32
    UINT64  EarlyPasses;                //  40
    UINT64  Runs;                       //  48
    UINT64  WorkQpcTicks;               //  56
    UINT64  LatenessBuckets[NF_VMIC_LATENESS_BUCKETS];  //  64
} NF_VMIC_STREAM_STATS, *PNF_VMIC_STREAM_STATS;

typedef struct _NF_VMIC_RING_STATS {
    UINT64  Attaches;                   //   0
    UINT64  Detaches;                   //   8
    UINT64  DetachCancel;               //  16: cancelled (CancelIoEx, or the thread or process exiting)
    UINT64  DetachCleanup;              //  24: the handle closed with the ATTACH pending
    UINT64  DetachRemoval;              //  32: the device going away
    UINT64  BusyRejects;                //  40
    UINT64  InvalidRejects;             //  48
    UINT64  Passes;                     //  56: capture passes that found a ring attached
    UINT64  FramesCopied;               //  64
    UINT64  UnderrunFrames;             //  72
    UINT64  UnderrunPackets;            //  80: passes with at least one underrun frame
    UINT64  Resyncs;                    //  88
    UINT64  MismatchPackets;            //  96: packets zero-filled because the stream wasn't 48 kHz float32 stereo (any stream)
    UINT64  NotReaderPackets;           // 104: passes of a capture stream other than the reader (served from the reader's copy)
    UINT64  FormatRejects;              // 112: ATTACHes refused with STATUS_NOT_SUPPORTED (a capture stream open in another format)
    UINT64  RefusedCaptureOpens;        // 120: capture stream creations refused while a ring was attached (not 48 kHz float32 stereo)
} NF_VMIC_RING_STATS, *PNF_VMIC_RING_STATS;

// The loopback ring's counters (Public.h, LOOPBACK_RING), widened to 64 bits.
typedef struct _NF_VMIC_LOOPBACK_STATS {
    UINT64  IdlePackets;                //   0
    UINT64  MismatchPackets;            //   8
    UINT64  UnderrunPackets;            //  16
    UINT64  UnderrunFrames;             //  24
    UINT64  OverrunPackets;             //  32
    UINT64  CaptureAnchors;             //  40
    UINT64  RenderRestarts;             //  48
    UINT64  RenderGapFills;             //  56
    UINT64  RenderLatePackets;          //  64
    UINT64  RenderLostPackets;          //  72
    UINT64  RenderNotOwner;             //  80
    UINT64  RefusedOpens;               //  88: stream creations the loopback refused: a format other than one open on the other
                                        //      circuit, or (not expected) no free format slot on its own
} NF_VMIC_LOOPBACK_STATS, *PNF_VMIC_LOOPBACK_STATS;

typedef struct _NF_VMIC_CAPTURE_TRACE {
    UINT64  PacketQueries;              //   0
    UINT64  LastQueryQpc;               //   8
    UINT64  LastQueryPacket;            //  16
    UINT64  LastQueryPacketQpc;         //  24
    UINT64  LastNotifyPacket;           //  32
    UINT64  LastNotifyQpc;              //  40
    UINT64  PositionQueries;            //  48
    UINT64  CurrentPacketQueries;       //  56
} NF_VMIC_CAPTURE_TRACE, *PNF_VMIC_CAPTURE_TRACE;

typedef struct _NF_VMIC_STATS {
    UINT32                  Magic;                      //   0
    UINT32                  Version;                    //   4
    UINT32                  StatsBytes;                 //   8
    UINT32                  Flags;                      //  12
    UINT64                  QpcFrequency;               //  16
    UINT64                  QpcNow;                     //  24
    UINT32                  AttachedPid;                //  32
    UINT32                  AttachedCapacityFrames;     //  36
    UINT64                  AttachedReadFrame;          //  40
    UINT64                  AttachedWriteFrame;         //  48
    UINT32                  ReaderPacketFrames;         //  56
    UINT32                  Reserved0;                  //  60
    NF_VMIC_RING_STATS      Ring;                       //  64
    NF_VMIC_LOOPBACK_STATS  Loopback;                   // 192
    UINT64                  SecondaryFramesCopied;      // 288
    UINT64                  SecondaryUnderrunFrames;    // 296
    UINT64                  ReaderHandoffs;             // 304
    UINT64                  HandoffSkippedFrames;       // 312
    NF_VMIC_STREAM_STATS    Stream[NF_VMIC_KINDS];      // 320
    NF_VMIC_CAPTURE_TRACE   CaptureTrace;               // 576
    UINT64                  SecondarySkips;             // 640
    UINT64                  SecondarySkippedFrames;     // 648
    UINT64                  LoopbackLeadSkips;          // 656
    UINT64                  LoopbackLeadSkippedFrames;  // 664
} NF_VMIC_STATS, *PNF_VMIC_STATS;

C_ASSERT(sizeof(NF_VMIC_ATTACH_IN) == 16);
C_ASSERT(FIELD_OFFSET(NF_VMIC_ATTACH_IN, Version) == 4);
C_ASSERT(NF_VMIC_MIN_RING_BYTES == 8448);
C_ASSERT(NF_VMIC_MAX_RING_BYTES == 2097408);
C_ASSERT(sizeof(NF_VMIC_RING_HEADER) == NF_VMIC_HEADER_BYTES);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_HEADER, Channels) == 20);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_HEADER, TotalBytes) == 32);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_HEADER, WriteFrame) == 64);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_HEADER, ReadFrame) == NF_VMIC_DRIVER_LINE_OFFSET);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_HEADER, Resyncs) == 152);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_HEADER, State) == 160);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_HEADER, Passes) == 168);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_HEADER, Reserved3) == NF_VMIC_DRIVER_LINE_OFFSET + NF_VMIC_LINE_BYTES);
C_ASSERT(sizeof(NF_VMIC_STREAM_STATS) == 128);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STREAM_STATS, LatenessBuckets) == 64);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_STATS, FormatRejects) == 112);
C_ASSERT(FIELD_OFFSET(NF_VMIC_RING_STATS, RefusedCaptureOpens) == 120);
C_ASSERT(sizeof(NF_VMIC_RING_STATS) == 128);
C_ASSERT(sizeof(NF_VMIC_LOOPBACK_STATS) == 96);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, Ring) == 64);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, Loopback) == 192);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, SecondaryFramesCopied) == 288);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, SecondaryUnderrunFrames) == 296);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, ReaderHandoffs) == 304);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, HandoffSkippedFrames) == 312);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, Stream) == 320);
C_ASSERT(sizeof(NF_VMIC_CAPTURE_TRACE) == 64);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, CaptureTrace) == 576);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, SecondarySkips) == 640);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, SecondarySkippedFrames) == 648);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, LoopbackLeadSkips) == 656);
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, LoopbackLeadSkippedFrames) == 664);
C_ASSERT(sizeof(NF_VMIC_STATS) == 672);
C_ASSERT(NF_VMIC_IOCTL_ATTACH == 0x0022E002);
C_ASSERT(NF_VMIC_IOCTL_STATS == 0x00226004);

#endif // NF_VMIC_H
