//! Which of our driver packages a setup action installs (TODO 19, the per-cable design, section 9). Pure.
//!
//! **The package rule (ours):** `add` and `variant` install the package the present cables run, their
//! `DEVPKEY_Device_DriverInfPath` and DriverVer, found among the drivers compatible with the new node; only `update` changes
//! it. If our cables run different packages, they refuse and say to run `update`. If a newer package of ours is staged, they
//! say so and go on with the running one. With no cable running a package, they take the newest of ours, as Plug and Play
//! ranks them. Every cable runs one service and one image: a cable installed from another package would run the image already
//! loaded while its DriverInfPath named the new one.

use std::fmt;

use crate::sessions::Mic;

/// The INF's file name as built; the driver store publishes it as `oemNN.inf` and remembers this one.
pub const INF_NAME: &str = "audiocodec.inf";

/// A compatible driver the search found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The published INF: `C:\Windows\INF\oem12.inf`.
    pub inf: String,
    /// The INF's name before the driver store published it (`audiocodec.inf`), or why it couldn't be read.
    pub original: Result<String, String>,
    pub provider: String,
    pub description: String,
    /// The INF's DriverVer date, as a FILETIME's 100 ns ticks since 1601.
    pub date: u64,
    /// The INF's DriverVer version, four 16-bit parts.
    pub version: u64,
    /// The microphones it makes for the hardware ID it was found for, by its INF, or why they couldn't be read.
    pub mics: Result<Vec<Mic>, String>,
}

impl Candidate {
    /// Whether it's our INF (by its original name).
    pub fn is_ours(&self) -> bool {
        self.original.as_deref().is_ok_and(|n| n.eq_ignore_ascii_case(INF_NAME))
    }

    /// Whether it makes all of `mics`, by its INF: `None` if that couldn't be read.
    pub fn makes(&self, mics: &[Mic]) -> Option<bool> {
        self.mics.as_ref().ok().map(|has| mics.iter().all(|m| has.contains(m)))
    }

    /// "makes the Microphone and the Call Microphone", "makes no microphone", or why its INF couldn't be read.
    pub fn mics_note(&self) -> String {
        match &self.mics {
            Ok(mics) if mics.is_empty() => "makes no microphone".into(),
            Ok(mics) => format!("makes {}", mics.iter().map(|m| m.describe()).collect::<Vec<_>>().join(" and ")),
            Err(why) => format!("its microphones unread: {why}"),
        }
    }

    /// "C:\Windows\INF\oem12.inf (audiocodec.inf), VS_Microsoft, 1.0.0.0 of 2026-11-23".
    pub fn describe(&self) -> String {
        let original = match &self.original {
            Ok(name) => name.clone(),
            Err(why) => format!("original name unknown: {why}"),
        };
        format!("{} ({original}), {}, {} of {}", self.inf, self.provider, version_text(self.version), date_text(self.date))
    }

    /// Its rank among ours: DriverVer's date, then its version, as Plug and Play ranks equal matches.
    pub fn rank(&self) -> (u64, u64) {
        (self.date, self.version)
    }
}

/// Our driver among the candidates: the newest by date, then by version; the first of those on a tie.
pub fn choose(candidates: &[Candidate]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, c) in candidates.iter().enumerate().filter(|(_, c)| c.is_ours()) {
        if best.is_none_or(|b| c.rank() > candidates[b].rank()) {
            best = Some(i);
        }
    }
    best
}

/// A search's drivers (or its error) as the newest of ours ([`choose`]) and the rest of ours, ranked below it.
pub fn rank<E>(found: Result<Vec<Candidate>, E>) -> (Result<Option<Candidate>, E>, Vec<Candidate>) {
    match found {
        Err(e) => (Err(e), Vec::new()),
        Ok(all) => {
            let best = choose(&all);
            let others = all.iter().enumerate().filter(|&(i, c)| Some(i) != best && c.is_ours()).map(|(_, c)| c.clone()).collect();
            (Ok(best.map(|i| all[i].clone())), others)
        }
    }
}

/// Whether a device whose driver Windows reports as `inf` (`DEVPKEY_Device_DriverInfPath`: `oem12.inf`) and `version`
/// (`DEVPKEY_Device_DriverVersion`) runs the package `c`: the same published INF, any case, and the same version. The version
/// too, because the driver store reuses an `oemNN.inf` name once its package is deleted.
pub fn runs(inf: Option<&str>, version: Option<&str>, c: &Candidate) -> bool {
    let (Some(inf), Some(version)) = (inf, version) else { return false };
    file_name(inf.trim()).eq_ignore_ascii_case(file_name(&c.inf)) && parse_version(version) == Some(c.version)
}

/// A path's file name: what follows the last `\` or `/`.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// The package a device node runs, as Windows reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    /// `oem12.inf`.
    pub inf: String,
    /// "12.40.56.342".
    pub version: String,
}

impl Running {
    /// The same package as `other`: the INF's name without case, the version as numbers.
    fn same(&self, other: &Running) -> bool {
        file_name(&self.inf).eq_ignore_ascii_case(file_name(&other.inf)) && parse_version(&self.version) == parse_version(&other.version)
    }

    pub fn runs(&self, c: &Candidate) -> bool {
        runs(Some(&self.inf), Some(&self.version), c)
    }
}

impl fmt::Display for Running {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.inf, self.version)
    }
}

/// The package a setup action installs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// The one every cable runs (`candidate` in the new node's compatible drivers); `newer`, a newer one of ours staged.
    Running { candidate: usize, newer: Option<usize> },
    /// No cable runs a package: the newest of ours.
    Newest(usize),
}

impl Pick {
    pub fn index(&self) -> usize {
        match *self {
            Pick::Running { candidate, .. } => candidate,
            Pick::Newest(i) => i,
        }
    }
}

/// Why no package can be picked. Nothing was changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageError {
    /// Our cables run different packages: each, with the instance IDs on it. `update` moves them to one.
    Mixed(Vec<(Running, Vec<String>)>),
    /// The cables' package isn't among the drivers compatible with the new node's hardware ID.
    NotCompatible { running: Running, hardware_id: String },
    /// No package of ours in the driver store has the hardware ID.
    NoneStaged { hardware_id: String },
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageError::Mixed(groups) => {
                let list: Vec<String> = groups.iter().map(|(r, ids)| format!("{r} on {}", ids.join(", "))).collect();
                write!(
                    f,
                    "our cables run different driver packages ({}): run `acx-ll-cable update <package folder>` to move every cable to \
                     one, then this again. Nothing was changed.",
                    list.join("; ")
                )
            }
            PackageError::NotCompatible { running, hardware_id } => write!(
                f,
                "the cables run {running}, but it isn't among the drivers compatible with {hardware_id}: run `acx-ll-cable update \
                 <package folder>` with a package that has it. Nothing was changed."
            ),
            PackageError::NoneStaged { hardware_id } => write!(
                f,
                "no driver package of ours ({INF_NAME}) in the driver store has {hardware_id}: add the package (pnputil /add-driver \
                 AudioCodec.inf, from an administrator prompt in its folder), then this again. Nothing was changed."
            ),
        }
    }
}

impl std::error::Error for PackageError {}

/// The package rule (the module's docs): `running`, the packages our present cables run (one entry a node: its instance ID
/// and package; nodes without a driver left out); `candidates`, the drivers compatible with the new node.
pub fn pick(running: &[(String, Running)], candidates: &[Candidate], hardware_id: &str) -> Result<Pick, PackageError> {
    let mut groups: Vec<(Running, Vec<String>)> = Vec::new();
    for (id, r) in running {
        match groups.iter_mut().find(|(g, _)| g.same(r)) {
            Some((_, ids)) => ids.push(id.clone()),
            None => groups.push((r.clone(), vec![id.clone()])),
        }
    }
    let newest = choose(candidates);
    match groups.as_slice() {
        [] => newest.map(Pick::Newest).ok_or_else(|| PackageError::NoneStaged { hardware_id: hardware_id.into() }),
        [(r, _)] => {
            let Some(candidate) = candidates.iter().position(|c| c.is_ours() && r.runs(c)) else {
                return Err(PackageError::NotCompatible { running: r.clone(), hardware_id: hardware_id.into() });
            };
            let newer = newest.filter(|&n| candidates[n].rank() > candidates[candidate].rank());
            Ok(Pick::Running { candidate, newer })
        }
        _ => Err(PackageError::Mixed(groups)),
    }
}

/// A DriverVer version's four parts: "1.0.0.0".
pub fn version_text(v: u64) -> String {
    format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xFFFF, (v >> 16) & 0xFFFF, v & 0xFFFF)
}

/// A driver version as Windows gives it ("12.40.56.342"): one to four parts of 0-65535, the missing ones 0, as DriverVer's
/// four 16-bit parts. Compared as numbers, so "1.02" is 1.2.0.0.
pub fn parse_version(s: &str) -> Option<u64> {
    let mut parts = s.trim().split('.');
    let mut v = 0u64;
    for i in 0..4 {
        let part = match parts.next() {
            Some(p) => p.trim().parse::<u16>().ok()?,
            None if i > 0 => 0,
            None => return None,
        };
        v = (v << 16) | u64::from(part);
    }
    parts.next().is_none().then_some(v)
}

/// A FILETIME's date (100 ns ticks since 1601-01-01 UTC) as "2026-11-23".
pub fn date_text(filetime: u64) -> String {
    const TICKS_PER_DAY: u64 = 864_000_000_000;
    // 1601-01-01 to 1970-01-01
    const DAYS_1601_TO_1970: i64 = 134_774;
    let days = (filetime / TICKS_PER_DAY) as i64 - DAYS_1601_TO_1970;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const V1: u64 = 1 << 48;

    /// One of our packages (by its original name) with both microphones for its hardware ID.
    pub(crate) fn candidate(inf: &str, original: &str, date: u64, version: u64) -> Candidate {
        Candidate {
            inf: inf.into(),
            original: Ok(original.into()),
            provider: "VS_Microsoft".into(),
            description: "ACX-LL-Cable".into(),
            date,
            version,
            mics: Ok(vec![Mic::LowLatency, Mic::Call]),
        }
    }

    pub(crate) fn running(inf: &str, version: &str) -> Running {
        Running { inf: inf.into(), version: version.into() }
    }

    #[test]
    fn our_newest_driver_is_chosen() {
        let older = candidate(r"C:\Windows\INF\oem3.inf", "AudioCodec.inf", 100, V1);
        let newer = candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 200, V1);
        let foreign = candidate(r"C:\Windows\INF\oem9.inf", "othercodec.inf", 900, V1);
        assert_eq!(choose(&[older.clone(), newer.clone(), foreign.clone()]), Some(1));
        assert_eq!(choose(&[newer.clone(), older.clone()]), Some(0));
        let bumped = candidate(r"C:\Windows\INF\oem13.inf", "audiocodec.inf", 200, V1 + 1);
        assert_eq!(choose(&[newer.clone(), bumped]), Some(1), "the same date: the higher version");
        assert_eq!(choose(&[newer.clone(), newer.clone()]), Some(0), "a tie: the first");
        assert_eq!(choose(std::slice::from_ref(&foreign)), None);
        assert_eq!(choose(&[]), None);
        let unknown = Candidate { original: Err("SetupGetInfInformationW failed".into()), ..newer.clone() };
        assert!(!unknown.is_ours());
        assert_eq!(choose(&[unknown]), None);
        assert_eq!(rank::<()>(Ok(vec![older.clone(), newer.clone(), foreign.clone()])), (Ok(Some(newer)), vec![older]));
        assert_eq!(rank::<()>(Ok(vec![foreign])), (Ok(None), vec![]));
        assert_eq!(rank::<u32>(Err(5)), (Err(5), vec![]));
    }

    #[test]
    fn a_device_runs_a_package_by_its_inf_and_version() {
        let c = candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 200, V1);
        assert!(runs(Some("oem12.inf"), Some("1.0.0.0"), &c));
        assert!(runs(Some(r" C:\WINDOWS\INF\OEM12.inf "), Some("1.0"), &c), "any case, a path; the version as numbers");
        assert!(!runs(Some("oem12.inf"), Some("1.0.0.300"), &c));
        assert!(!runs(Some("oem1.inf"), Some("1.0.0.0"), &c), "the whole name, not a prefix");
        assert!(!runs(Some("oem12.inf"), None, &c) && !runs(None, Some("1.0.0.0"), &c));
        assert!(!runs(Some("oem12.inf"), Some("one"), &c));
    }

    /// The package rule with two cables present (the design's section 9).
    #[test]
    fn add_and_variant_take_the_package_the_cables_run() {
        let a = (r"ROOT\MEDIA\ACXLLCABLE_A_X_1".to_string(), running("oem12.inf", "3.27.41.512"));
        let b = (r"ROOT\MEDIA\ACXLLCABLE_B_Y_1".to_string(), running("OEM12.INF", "3.27.41.512"));
        let v = parse_version("3.27.41.512").unwrap();
        let runs_now = candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 200, v);
        let staged_newer = candidate(r"C:\Windows\INF\oem15.inf", "audiocodec.inf", 300, V1);
        let staged_older = candidate(r"C:\Windows\INF\oem3.inf", "audiocodec.inf", 100, V1);
        let foreign = candidate(r"C:\Windows\INF\oem9.inf", "othercodec.inf", 900, V1);
        let hw = r"ROOT\AcxLlCable";
        // Both run one package: it, though a newer one is staged (said, not taken)
        let all = [staged_older.clone(), runs_now.clone(), staged_newer.clone(), foreign.clone()];
        assert_eq!(pick(&[a.clone(), b.clone()], &all, hw), Ok(Pick::Running { candidate: 1, newer: Some(2) }));
        assert_eq!(pick(&[a.clone(), b.clone()], &all[..2], hw), Ok(Pick::Running { candidate: 1, newer: None }));
        // They run different packages: refused, naming each
        let b_old = (b.0.clone(), running("oem3.inf", "1.0.0.0"));
        let Err(PackageError::Mixed(groups)) = pick(&[a.clone(), b_old.clone()], &all, hw) else { panic!() };
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1], (b_old.1.clone(), vec![b_old.0.clone()]));
        let text = PackageError::Mixed(groups).to_string();
        assert!(text.contains("oem12.inf 3.27.41.512 on ROOT\\MEDIA\\ACXLLCABLE_A_X_1; oem3.inf 1.0.0.0 on"), "{text}");
        assert!(text.contains("acx-ll-cable update") && text.ends_with("Nothing was changed."), "{text}");
        // The running package isn't compatible with the hardware ID asked for (or is another INF of the same name)
        assert_eq!(
            pick(std::slice::from_ref(&a), std::slice::from_ref(&staged_newer), hw),
            Err(PackageError::NotCompatible { running: a.1.clone(), hardware_id: hw.into() })
        );
        let foreign_same_name = Candidate { original: Ok("othercodec.inf".into()), ..runs_now.clone() };
        assert!(matches!(pick(std::slice::from_ref(&a), &[foreign_same_name], hw), Err(PackageError::NotCompatible { .. })));
        // No cable running a package: the newest
        assert_eq!(pick(&[], &all, hw), Ok(Pick::Newest(2)));
        assert_eq!(pick(&[], &[foreign], hw), Err(PackageError::NoneStaged { hardware_id: hw.into() }));
        assert_eq!(Pick::Newest(2).index(), 2);
    }

    #[test]
    fn versions_and_dates_read_as_windows_shows_them() {
        assert_eq!(parse_version("12.40.56.342"), Some((12 << 48) | (40 << 32) | (56 << 16) | 342));
        assert_eq!(parse_version("1.02.0.0"), Some((1 << 48) | (2 << 32)));
        assert_eq!(parse_version("1"), Some(1 << 48));
        for bad in ["", " ", "1.2.3.4.5", "65536.0.0.0", "1..2", "a.b", "-1.0", "1.0."] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        for v in [0, V1, (12 << 48) | (40 << 32) | (56 << 16) | 342, u64::MAX] {
            assert_eq!(parse_version(&version_text(v)), Some(v));
        }
        assert_eq!(date_text(0), "1601-01-01");
        assert_eq!(date_text(116_444_736_000_000_000), "1970-01-01");
        assert_eq!(date_text(116_444_736_000_000_000 + (19_782 * 864_000_000_000)), "2024-02-29");
        let c = candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 116_444_736_000_000_000, V1);
        assert_eq!(c.describe(), r"C:\Windows\INF\oem12.inf (audiocodec.inf), VS_Microsoft, 1.0.0.0 of 1970-01-01");
    }
}
