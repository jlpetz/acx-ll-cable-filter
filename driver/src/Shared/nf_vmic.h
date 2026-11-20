/*++

    noise-filter (TODO 19, spike checks 4 and 5): the protocol between the app and the virtual microphone driver.

    Original to noise-filter, not from Microsoft's sample: MIT OR Apache-2.0, like the Rust crate that mirrors it
    (crates/nf-vmic/src/protocol.rs, which checks these offsets at compile time). Change the two together, and bump
    NF_VMIC_VERSION on any change to a layout or a rule.

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
          - STATUS_REVISION_MISMATCH (ERROR_REVISION_MISMATCH, 1306): the input's or the header's Version isn't NF_VMIC_VERSION.
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
      frames copied, and stores ReadFrame with release ordering after the copy. Every frame the app writes is delivered once
      and in order; an underrun inserts silence and drops nothing; an app that keeps 0 <= fill <= capacity never sees a re-sync.
    - The capacity, and the fill the app keeps, must be at least one capture packet (PacketFrames: 480 frames for a shared
      stream's 10 ms period), or every pass underruns. NF_VMIC_MIN_CAPACITY_FRAMES holds two such packets.
    - One capture stream holds the reader slot at a time: the first to run after ATTACH (or after the last one paused). Only
      the reader reads the ring and moves ReadFrame, so its passes set the pace. The driver keeps a copy of the packets it
      delivers (its underrun silence included) on the stream engine's QPC time line, and every other capture stream running
      in 48 kHz float32 stereo plays that copy a fixed lag behind: the reader's packet plus 1 ms and a frame. More than one
      capture stream can run at once: the microphones of two of the driver's devices share the one app ring, and the audio
      engine can open a device stream per processing mode should the capture pin list more than RAW (it lists only RAW).
      The other streams' passes are counted in NF_VMIC_STATS only (NotReaderPackets, SecondaryFramesCopied,
      SecondaryUnderrunFrames). A handoff (the reader pauses and another stream takes the slot) can skip or repeat up to
      that lag once in the other streams, and leave a short gap of silence in them. A jump in a stream's time line (a late
      timer's glitch adjustment, Stream[NF_VMIC_KIND_CAPTURE].GlitchPasses) moves the copy's audio by its length: the
      reader's inserts that much silence in the other streams (its own output doesn't show it), another stream's skips
      that much of the copy.
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
       4     4  Version          app     NF_VMIC_VERSION
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
     184     8  Reserved2        driver  0
    Line 3 (192-255): reserved, 0.
     256        frames           app     CapacityFrames x NF_VMIC_FRAME_BYTES

NF_VMIC_STATS (576 bytes; all counters cumulative since the driver loaded; times in 100 ns units):

    Offset Size Field                    Content
    ------ ---- ------------------------ -----------------------------------------------------------------------------
       0     4  Magic, 4 Version, 8 StatsBytes (576), 12 Flags (NF_VMIC_STATS_*)
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
     288     8  SecondaryFramesCopied    frames of the ring's audio delivered to capture streams other than the reader
     296     8  SecondaryUnderrunFrames  frames such a stream zero-filled because the ring's audio for its position wasn't there
     304    16  Reserved1
     320   256  Stream[2]                NF_VMIC_STREAM_STATS: [NF_VMIC_KIND_RENDER], [NF_VMIC_KIND_CAPTURE]

    The reader (with NF_VMIC_STATS_ATTACHED): Flags' READING and FORMAT_MISMATCH are the State bits the stream holding the
    reader slot wrote to the header at its latest pass, and ReaderPacketFrames is the PacketFrames it wrote, so STATS and the
    header agree. While no stream holds the slot all three are 0. A stream in another format holds the slot without reading:
    FORMAT_MISMATCH, and PacketFrames in its own format's frames (0 if the driver can't count its packet in frames).

    The other capture streams (the ring's rules): SecondaryFramesCopied counts frames the reader delivered, copied into
    their packets from the reader's copy, its underrun silence included (the ring's UnderrunFrames counts that).
    SecondaryUnderrunFrames counts the frames of silence they got because no reader delivered audio for their position: no
    reader pass in the ring's format has reached it yet (the start of an attachment, a reader in another format), the
    reader's time line jumped past it (a handoff, or a glitch adjustment: the copy holds silence there), or their lag and
    packet exceed the copy (0.68 s). The driver remembers the latest 16 such jumps, which covers a 10 ms stream beside a
    2 ms reader that jumps at every pass; beyond that, the older ones' silence counts as copied. A stream other than the
    reader in another format gets silence, counted in the ring's MismatchPackets. Ring.NotReaderPackets counts all their
    passes.

    NF_VMIC_STREAM_STATS (128 bytes), per stream kind: TimerPasses (passes the stream's timer started), GlitchPasses (passes
    run at once because the next was already due: each is one glitch adjustment of the stream's time line), GlitchHns (their
    sum), LatenessMaxHns, LatenessSumHns, EarlyPasses (timer passes before their time: rounding, expected 0), Runs
    (Pause-to-Run), Reserved, LatenessBuckets[8]. A timer pass's lateness is the time it ran (its QPC, in 100 ns units)
    minus the time ScheduleNextPass armed the timer for. Buckets: below 50 us, 50-100, 100-250, 250-500 us, 0.5-1 ms, 1-2 ms,
    2-5 ms, and 5 ms and over (NF_VMIC_LATENESS_BOUNDS_HNS are the upper bounds of the first seven).

--*/

#ifndef NF_VMIC_H
#define NF_VMIC_H
#pragma once

#define NF_VMIC_MAGIC                   0x4D56464Eu     // "NFVM" in memory order
#define NF_VMIC_VERSION                 1u

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
#define NF_VMIC_STATE_ATTACHED          0x00000001u     // the driver published this ring
#define NF_VMIC_STATE_READING           0x00000002u     // the stream holding the reader slot read it at its latest pass (underrun or not)
#define NF_VMIC_STATE_FORMAT_MISMATCH   0x00000004u     // that stream isn't 48 kHz float32 stereo: silence, the ring not read

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
    UINT32  Version;                    //   4: NF_VMIC_VERSION
    UINT32  InBytes;                    //   8: sizeof(NF_VMIC_ATTACH_IN)
    UINT32  Flags;                      //  12: 0
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
    UINT64  Reserved2;                  // 184
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
    UINT64  Reserved;                   //  56
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
    UINT64  Reserved;                   //  88
} NF_VMIC_LOOPBACK_STATS, *PNF_VMIC_LOOPBACK_STATS;

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
    UINT64                  Reserved1[2];               // 304
    NF_VMIC_STREAM_STATS    Stream[NF_VMIC_KINDS];      // 320
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
C_ASSERT(FIELD_OFFSET(NF_VMIC_STATS, Stream) == 320);
C_ASSERT(sizeof(NF_VMIC_STATS) == 576);
C_ASSERT(NF_VMIC_IOCTL_ATTACH == 0x0022E002);
C_ASSERT(NF_VMIC_IOCTL_STATS == 0x00226004);

#endif // NF_VMIC_H
