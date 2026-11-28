//! acx-ll-cable: the cables' setup tool (TODO 19, the per-cable design, section 9). A thin command line over nf-vmic's
//! `setup` and `variant`; no audio, no app, no broker, so a user of plain cables needs nothing else. `list` is for any user;
//! the rest needs an elevated administrator (the setup's lock refuses anyone else before anything is read).
//!
//! Exit codes: 0 done (or nothing to do), 1 usage, 2 an error (the message says what changed, if anything), 7 Windows needs a
//! restart to finish, with or without other problems beside it (stop there: restart, then run it again, which says what is still
//! wrong); `endpoint` only: 3 the cable has no endpoint for that role now (stderr says why). `migrate` and `add` take
//! `--endpoint-map <file>`: the old endpoints' new ones, as JSON (`nf_vmic::setup::endpoint_map`).
#![warn(clippy::undocumented_unsafe_blocks)]

use std::path::PathBuf;
use std::process::ExitCode;

use nf_vmic::cables::EndpointKind;
use nf_vmic::identity::{CableId, Label};
use nf_vmic::roles::{Resolved, Snapshot};
use nf_vmic::setup::plan::RemoveTarget;
use nf_vmic::setup::{self, Done, SetupError};
use nf_vmic::variant::{self, Outcome, Variant};

const USAGE: &str = "\
acx-ll-cable: ACX-LL-Cable's setup (each cable has its own In and recording outputs)

  acx-ll-cable list
      The present cables, their devices and endpoints, the remembered absent ones, and setup errors. A disabled cable's line
      ends \": disabled\"; \": disabled at the next restart\" while a disable Windows couldn't finish waits for one (the
      cable still works); \": disabled, not in every hardware profile\" for a stopped setup run's, or another tool's for this
      profile only (devcon's). Any user.
  acx-ll-cable endpoint <label|id> In|Out|Out-5ms (also speakers|microphone|call-microphone)
      The cable's endpoint for that role as Windows has it now, found by the cable's identity and the endpoint's pin, never by
      name: one line, its id, its state (active, disabled in the Sound panel, unplugged) and its name in quotes. None now
      (removed, disabled, not working, not made yet, or a role its microphones lack): exit 3, why on stderr. Any user.
  acx-ll-cable add <label> [--mics both|call|low-latency] [--adopt <instance ID>] [--endpoint-map <file>]
      Makes cable <label> (one to eight of A-Z and 0-9), on the driver package the other cables run. Present and working, or
      disabled: no change (`enable` turns a disabled one on). Present but not working: repaired in place. Removed earlier: back
      with its identity and, with the microphones it had, its instance ID (Windows makes it new endpoint ids). --mics: its
      microphones (default both, or what it had). --adopt: re-creates a device made outside this tool as the cable (only then:
      `list` shows such devices).
  acx-ll-cable remove <label> [--forget]
  acx-ll-cable remove --all [--forget]
      Removes the cable's device (every device of ours with --all, the old driver's too). Its identity stays, so `add` brings
      it back with its identity (its endpoints new), unless --forget.
  acx-ll-cable disable <label>
  acx-ll-cable enable <label>
      Turns the cable off, keeping it installed, across restarts too, until `enable` turns it back on; its endpoints keep their
      ids (not present while it is off). No other cable is touched. `enable` checks the endpoints came back as they were. A
      disable Windows can't finish now (an app has the cable in use) is made at the next restart (exit 7): until then the
      cable works, and `enable` cancels it.
  acx-ll-cable variant <label> both|call|low-latency
      Switches the cable's microphones: its device re-created with new endpoints; no other cable is touched. A disabled
      cable is refused (exit 2): `enable` it first, then switch, then `disable` it again to keep it off.
  acx-ll-cable update <package folder> [--endpoint-map <file>]
      Moves every cable to the package in the folder (driver\\out\\Release\\AudioCodec): every cable stops for a few seconds.
      If the old driver image doesn't unload (an app has a cable's control device open), every cable is put back as it was.
      Windows may replace endpoint IDs. The requested map records the endpoints observed before and after this update.
  acx-ll-cable migrate <package folder> [--a-mics m] [--b-mics m] [--endpoint-map <file>]
      From the old driver's devices (ROOT\\AudioCodec as A, ROOT\\NoiseFilterCableB as B) to cables on the package in the
      folder. A keeps its variant unless --a-mics; B gets --b-mics (default both). Nothing changes unless the package makes
      both; A and B are recorded first, so a run that stops part way is finished by running this again.

  --endpoint-map <file> (migrate, add, update): writes which new endpoint takes each old one's place (by cable and role, with the
      names), as JSON, published whole at the end (a new temporary file of its own beside it, flushed, renamed over it) once
      the endpoints before the run were read; a run that publishes none leaves the file as it was. Each run maps the cables
      it makes or checks and those a migration made whose map isn't final; the run that leaves nothing pending maps all of
      it, whatever runs came before. Close the file before a run ends: one open in another process is waited for about a
      second, then the run is refused at the start (nothing changed) or its map not published (the file as it was).

Exit codes: 0 done, 1 usage, 2 an error, 7 Windows needs a restart to finish, other errors beside it or not (stop there:
restart, then run this again, which says what is still wrong); `endpoint`: 3 no endpoint for that role now.";

/// A parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cmd {
    Help,
    List,
    Add { label: Label, mics: Option<Variant>, adopt: Option<String>, map: Option<PathBuf> },
    Remove { target: RemoveTarget, forget: bool },
    Disable { label: Label },
    Enable { label: Label },
    Endpoint { cable: CableArg, role: EndpointKind },
    Variant { label: Label, target: Variant },
    Update { folder: PathBuf, map: Option<PathBuf> },
    Migrate { folder: PathBuf, a_mics: Option<Variant>, b_mics: Option<Variant>, map: Option<PathBuf> },
}

/// `endpoint`'s cable: an identity (36 characters), or a present cable's label.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CableArg {
    Id(CableId),
    Label(Label),
}

fn cable_arg(v: Option<&String>) -> Result<CableArg, String> {
    let v = v.ok_or("'endpoint' takes a cable's label or id, and a role: In, Out or Out-5ms")?;
    match v.parse::<CableId>() {
        Ok(id) => Ok(CableArg::Id(id)),
        Err(_) => v.parse().map(CableArg::Label).map_err(|e| format!("{e} (nor a cable's id)")),
    }
}

fn mics(flag: &str, v: Option<&String>) -> Result<Variant, String> {
    let v = v.ok_or_else(|| format!("{flag} takes both, call or low-latency"))?;
    Variant::from_arg(v).ok_or_else(|| format!("{flag} takes both, call or low-latency, not '{v}'"))
}

fn label(v: Option<&String>, what: &str) -> Result<Label, String> {
    v.ok_or_else(|| format!("'{what}' takes a cable's label"))?.parse().map_err(|e| format!("{e}"))
}

/// The command line, without the program's name.
fn parse(args: &[String]) -> Result<Cmd, String> {
    let Some(command) = args.first() else { return Ok(Cmd::Help) };
    let rest = &args[1..];
    let mut flags = rest.iter().peekable();
    let mut positional: Vec<&String> = Vec::new();
    let mut opt_mics = None;
    let mut adopt = None;
    let mut forget = false;
    let mut all = false;
    let mut a_mics = None;
    let mut b_mics = None;
    let mut map: Option<PathBuf> = None;
    while let Some(a) = flags.next() {
        match a.as_str() {
            "--mics" => opt_mics = Some(mics(a, flags.next())?),
            "--a-mics" => a_mics = Some(mics(a, flags.next())?),
            "--b-mics" => b_mics = Some(mics(a, flags.next())?),
            "--adopt" => adopt = Some(flags.next().ok_or("--adopt takes a device's instance ID")?.clone()),
            "--endpoint-map" => map = Some(flags.next().ok_or("--endpoint-map takes a file")?.into()),
            "--forget" => forget = true,
            "--all" => all = true,
            f if f.starts_with("--") => return Err(format!("'{command}' doesn't take {f}")),
            _ => positional.push(a),
        }
    }
    let allowed = |ok: &[bool]| ok.iter().all(|&b| b);
    let cmd = match command.as_str() {
        "help" | "--help" | "-h" | "/?" => Cmd::Help,
        "list" => Cmd::List,
        "add" => Cmd::Add { label: label(positional.first().copied(), "add")?, mics: opt_mics, adopt: adopt.clone(), map: map.clone() },
        "remove" => {
            let target = match (all, positional.first()) {
                (true, None) => RemoveTarget::All,
                (false, Some(l)) => RemoveTarget::Label(label(Some(l), "remove")?),
                (true, Some(_)) => return Err("'remove' takes a label or --all, not both".into()),
                (false, None) => return Err("'remove' takes a cable's label, or --all".into()),
            };
            Cmd::Remove { target, forget }
        }
        "disable" => Cmd::Disable { label: label(positional.first().copied(), "disable")? },
        "enable" => Cmd::Enable { label: label(positional.first().copied(), "enable")? },
        "endpoint" => {
            let cable = cable_arg(positional.first().copied())?;
            let role = positional.get(1).ok_or("'endpoint' takes a role after the cable: In, Out or Out-5ms")?;
            let role = EndpointKind::from_arg(role).ok_or_else(|| format!("'endpoint' takes In, Out or Out-5ms, not '{role}'"))?;
            Cmd::Endpoint { cable, role }
        }
        "variant" => {
            let target = positional.get(1).ok_or("'variant' takes a label and both, call or low-latency")?;
            let target = Variant::from_arg(target).ok_or_else(|| format!("'variant' takes both, call or low-latency, not '{target}'"))?;
            Cmd::Variant { label: label(positional.first().copied(), "variant")?, target }
        }
        "update" => Cmd::Update { folder: positional.first().ok_or("'update' takes the package's folder")?.into(), map: map.clone() },
        "migrate" => {
            let folder = positional.first().ok_or("'migrate' takes the package's folder")?.into();
            Cmd::Migrate { folder, a_mics, b_mics, map: map.clone() }
        }
        other => return Err(format!("no command '{other}'")),
    };
    // What each command takes, and how many positional arguments
    let no_map = map.is_none();
    let none = allowed(&[opt_mics.is_none(), adopt.is_none(), !forget, !all, a_mics.is_none(), b_mics.is_none(), no_map]);
    let (max, ok) = match &cmd {
        Cmd::Help | Cmd::List => (0, none),
        Cmd::Disable { .. } | Cmd::Enable { .. } => (1, none),
        Cmd::Update { .. } => (1, allowed(&[opt_mics.is_none(), adopt.is_none(), !forget, !all, a_mics.is_none(), b_mics.is_none()])),
        Cmd::Endpoint { .. } | Cmd::Variant { .. } => (2, none),
        Cmd::Add { .. } => (1, allowed(&[!forget, !all, a_mics.is_none(), b_mics.is_none()])),
        Cmd::Remove { .. } => (1, allowed(&[opt_mics.is_none(), adopt.is_none(), a_mics.is_none(), b_mics.is_none(), no_map])),
        Cmd::Migrate { .. } => (1, allowed(&[opt_mics.is_none(), adopt.is_none(), !forget, !all])),
    };
    if positional.len() > max {
        return Err(format!("'{command}' takes {max} argument{}; '{}' is one more", if max == 1 { "" } else { "s" }, positional[max]));
    }
    if !ok {
        return Err(format!("'{command}' doesn't take that option"));
    }
    Ok(cmd)
}

fn say(line: &str) {
    println!("{line}");
}

/// The exit code of an action's result (the module's docs): 7 whenever Windows needs a restart to finish, other problems beside
/// it or not (they are said; the run after the restart says what is still wrong), so a caller can stop there.
fn code_of(r: &Result<Done, SetupError>) -> u8 {
    match r {
        Ok(Done::Nothing | Done::Changed) => 0,
        Ok(Done::RestartPending { .. }) => 7,
        Err(e) if e.needs_restart() => 7,
        Err(_) => 2,
    }
}

fn exit_of(r: Result<Done, SetupError>) -> ExitCode {
    let code = code_of(&r);
    match r {
        Ok(Done::Nothing) => say("Nothing to do."),
        Ok(Done::Changed) => say("Done."),
        Ok(Done::RestartPending { device }) => {
            say(&format!("Windows needs a restart to finish with {device}: restart, then run `acx-ll-cable list`."))
        }
        Err(e) => {
            eprintln!("error: {e}");
            if code == 7 {
                say("Windows needs a restart to finish: restart, then run this again.");
            }
        }
    }
    ExitCode::from(code)
}

/// `endpoint`'s answer from a snapshot (pure): the line it prints, `<id> <state> "<name>"` (exit 0); or its exit code and what
/// it says on stderr: 3 and "none: <why>" when the cable has no endpoint for the role now (not present among them), 2 for a
/// label on more than one identity (a setup error).
fn endpoint_answer(s: &Snapshot, cable: &CableArg, role: EndpointKind) -> Result<String, (u8, String)> {
    let id = match cable {
        CableArg::Id(id) => *id,
        CableArg::Label(l) => match s.cable_labelled(l) {
            Ok(Some(id)) => id,
            Ok(None) => return Err((3, format!("none: no cable labelled {l} is present (`acx-ll-cable list` shows the cables)"))),
            Err(ids) => {
                let ids: Vec<String> = ids.iter().map(CableId::to_string).collect();
                return Err((
                    2,
                    format!("error: more than one cable is labelled {l} ({}), a setup error: name it by its id", ids.join(", ")),
                ));
            }
        },
    };
    match s.resolve(id, role) {
        Resolved::Endpoint(e) => Ok(format!("{} {} \"{}\"", e.id, e.state.arg(), e.name)),
        Resolved::None(why) => Err((3, format!("none: {why}"))),
    }
}

fn main() -> ExitCode {
    let Ok(args) = std::env::args_os().skip(1).map(|a| a.into_string()).collect::<Result<Vec<String>, _>>() else {
        eprintln!("error: an argument isn't Unicode");
        return ExitCode::from(1);
    };
    let cmd = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(1);
        }
    };
    let mut out = say;
    match cmd {
        Cmd::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Cmd::List => match setup::read_report() {
            Ok(r) => {
                print!("{}", setup::format_list(&r));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        Cmd::Endpoint { cable, role } => match Snapshot::read() {
            Ok(s) => match endpoint_answer(&s, &cable, role) {
                Ok(line) => {
                    println!("{line}");
                    ExitCode::SUCCESS
                }
                Err((code, why)) => {
                    eprintln!("{why}");
                    ExitCode::from(code)
                }
            },
            Err(e) => {
                eprintln!("error: the cables or their endpoints couldn't be read: {e}");
                ExitCode::from(2)
            }
        },
        Cmd::Add { label, mics, adopt, map } => exit_of(setup::add(&label, mics, adopt.as_deref(), map.as_deref(), &mut out)),
        Cmd::Remove { target, forget } => exit_of(setup::remove(&target, forget, &mut out)),
        Cmd::Disable { label } => exit_of(setup::disable(&label, &mut out)),
        Cmd::Enable { label } => exit_of(setup::enable(&label, &mut out)),
        Cmd::Variant { label, target } => exit_of(variant::switch(&label, target, &mut out).map(|o| match o {
            Outcome::Nothing => Done::Nothing,
            Outcome::Switched => Done::Changed,
            Outcome::RestartPending { device } => Done::RestartPending { device },
        })),
        Cmd::Update { folder, map } => exit_of(setup::update_with_map(&folder, map.as_deref(), &mut out)),
        Cmd::Migrate { folder, a_mics, b_mics, map } => exit_of(setup::migrate(&folder, a_mics, b_mics, map.as_deref(), &mut out)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: &str) -> Result<Cmd, String> {
        parse(&line.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    fn l(s: &str) -> Label {
        s.parse().unwrap()
    }

    #[test]
    fn the_commands_parse() {
        assert_eq!(p(""), Ok(Cmd::Help));
        assert_eq!(p("list"), Ok(Cmd::List));
        assert_eq!(p("add d"), Ok(Cmd::Add { label: l("D"), mics: None, adopt: None, map: None }));
        assert_eq!(p("add B --mics call"), Ok(Cmd::Add { label: l("B"), mics: Some(Variant::Call), adopt: None, map: None }));
        assert_eq!(
            p(r"add B --endpoint-map C:\out\b.json"),
            Ok(Cmd::Add { label: l("B"), mics: None, adopt: None, map: Some(r"C:\out\b.json".into()) })
        );
        assert_eq!(
            p(r"add C --adopt ROOT\MEDIA\0007"),
            Ok(Cmd::Add { label: l("C"), mics: None, adopt: Some(r"ROOT\MEDIA\0007".into()), map: None })
        );
        assert_eq!(p("remove B"), Ok(Cmd::Remove { target: RemoveTarget::Label(l("B")), forget: false }));
        assert_eq!(p("remove B --forget"), Ok(Cmd::Remove { target: RemoveTarget::Label(l("B")), forget: true }));
        assert_eq!(p("remove --all --forget"), Ok(Cmd::Remove { target: RemoveTarget::All, forget: true }));
        assert_eq!(p("variant B low-latency"), Ok(Cmd::Variant { label: l("B"), target: Variant::LowLatency }));
        assert_eq!(p("disable b"), Ok(Cmd::Disable { label: l("B") }));
        assert_eq!(p("enable B"), Ok(Cmd::Enable { label: l("B") }));
        assert_eq!(
            p("endpoint B call-microphone"),
            Ok(Cmd::Endpoint { cable: CableArg::Label(l("B")), role: EndpointKind::CallMicrophone })
        );
        let id: CableId = "bbbbbbbb-0000-4000-8000-00000000000b".parse().unwrap();
        assert_eq!(p(&format!("endpoint {id} Speakers")), Ok(Cmd::Endpoint { cable: CableArg::Id(id), role: EndpointKind::Speakers }));
        assert_eq!(p(r"update C:\pkg"), Ok(Cmd::Update { folder: r"C:\pkg".into(), map: None }));
        assert_eq!(p(r"update C:\pkg --endpoint-map m.json"), Ok(Cmd::Update { folder: r"C:\pkg".into(), map: Some("m.json".into()) }));
        assert_eq!(p(r"migrate C:\pkg"), Ok(Cmd::Migrate { folder: r"C:\pkg".into(), a_mics: None, b_mics: None, map: None }));
        assert_eq!(
            p(r"migrate C:\pkg --a-mics call --b-mics low-latency --endpoint-map m.json"),
            Ok(Cmd::Migrate {
                folder: r"C:\pkg".into(),
                a_mics: Some(Variant::Call),
                b_mics: Some(Variant::LowLatency),
                map: Some("m.json".into())
            })
        );
    }

    /// The exit codes a harness stops on (Astra's review, 2026-11-25): 7 for every result that needs a restart, the migration's
    /// removal of an old node among them, with other problems beside it too.
    #[test]
    fn a_restart_windows_needs_is_exit_7() {
        use nf_vmic::win_error::WinError;
        let restart_first = || SetupError::RestartFirst { device: r"ROOT\MEDIA\0003".into() };
        let stopped = |error| Err(SetupError::Stopped { error: Box::new(error), unrestored: vec![], to_make: vec![l("A"), l("B")] });
        assert_eq!(code_of(&Ok(Done::Nothing)), 0);
        assert_eq!(code_of(&Ok(Done::Changed)), 0);
        assert_eq!(code_of(&Ok(Done::RestartPending { device: "X".into() })), 7);
        assert_eq!(code_of(&Err(restart_first())), 7);
        assert_eq!(code_of(&stopped(restart_first())), 7, "the old node's removal Windows finishes at the next restart");
        assert_eq!(code_of(&stopped(SetupError::Windows(WinError { call: "DiUninstallDevice", code: 5 }))), 2);
        let with_problems = SetupError::Unfinished { problems: vec!["x".into()], to_make: vec![l("B")] }.and_restart(Some("Y".into()));
        assert_eq!(code_of(&Err(with_problems)), 7, "a restart with other problems: a failed make's removal among them");
        let unfinished = SetupError::Unfinished { problems: vec!["B's endpoints".into()], to_make: vec![l("B")] };
        assert_eq!(code_of(&Err(unfinished)), 2, "a cable left pending, its endpoints not there");
        assert_eq!(code_of(&Err(SetupError::Checks(vec!["x".into()]))), 2);
        let still = SetupError::StillPresent { device: "X".into(), why: "w".into(), restart: true, error: Box::new(restart_first()) };
        assert_eq!(code_of(&Err(still)), 7);
        // The repair's install asked for a restart and the enable after it failed (Astra's review, 2026-11-26)
        let enable = SetupError::Windows(WinError { call: "CM_Enable_DevNode", code: 5 });
        assert_eq!(code_of(&Err(enable.clone().and_restart(Some(r"ROOT\MEDIA\ACXLLCABLE_B_X_1".into())))), 7);
        assert_eq!(code_of(&Err(enable)), 2);
    }

    /// `endpoint`'s answer: the id, state and name on one line (exit 0), or none (exit 3) and why; a label on two identities is a
    /// setup error (exit 2). By identity and pin, as nf_vmic::roles has it.
    #[test]
    fn the_endpoint_command_answers_by_identity_and_role() {
        use nf_vmic::identity::CableInstance;
        use nf_vmic::setup::endpoints::{Endpoint, State};
        use nf_vmic::setup::plan::{Health, Model, Node};
        let b_id: CableId = "bbbbbbbb-0000-4000-8000-00000000000b".parse().unwrap();
        let b = CableInstance { label: l("B"), id: b_id, n: 1 };
        let node = |c: &CableInstance, v: Variant| Node {
            instance_id: c.instance_id(),
            model: Model::Cable(v),
            cable: Some(c.clone()),
            health: Health::Working,
            running: None,
            start_attempts: None,
            problem_status: None,
            config_flags: Some(0),
        };
        let endpoint = |kind, id: &str| Endpoint {
            id: id.into(),
            name: format!("{} (ACX-LL-Cable B)", EndpointKind::name(kind)),
            kind,
            state: State::Active,
            instance_id: b.instance_id().to_lowercase(),
        };
        let s = Snapshot {
            nodes: vec![node(&b, Variant::LowLatency)],
            endpoints: vec![endpoint(EndpointKind::Speakers, "{0.0.0.00000000}.{b}")],
        };
        let by_label = CableArg::Label(l("B"));
        assert_eq!(
            endpoint_answer(&s, &by_label, EndpointKind::Speakers),
            Ok(r#"{0.0.0.00000000}.{b} active "Speakers (ACX-LL-Cable B)""#.into())
        );
        assert_eq!(
            endpoint_answer(&s, &CableArg::Id(b_id), EndpointKind::Speakers).unwrap().split(' ').next(),
            Some("{0.0.0.00000000}.{b}")
        );
        let (code, why) = endpoint_answer(&s, &by_label, EndpointKind::CallMicrophone).unwrap_err();
        assert_eq!(code, 3);
        assert!(why.starts_with("none: ACX-LL-Cable B has no Out-5ms"), "{why}");
        let (code, why) = endpoint_answer(&s, &by_label, EndpointKind::Microphone).unwrap_err();
        assert!(code == 3 && why.contains("Windows shows no Out for it (yet)"), "{why}");
        let (code, why) = endpoint_answer(&s, &CableArg::Label(l("C")), EndpointKind::Speakers).unwrap_err();
        assert!(code == 3 && why.starts_with("none: no cable labelled C is present"), "{why}");
        let gone: CableId = "cccccccc-0000-4000-8000-00000000000c".parse().unwrap();
        let (code, why) = endpoint_answer(&s, &CableArg::Id(gone), EndpointKind::Speakers).unwrap_err();
        assert!(code == 3 && why.starts_with("none: no cable with id cccccccc-"), "{why}");
        // Two identities labelled B
        let other = CableInstance { label: l("B"), id: gone, n: 1 };
        let two = Snapshot { nodes: vec![node(&b, Variant::Both), node(&other, Variant::Both)], endpoints: vec![] };
        let (code, why) = endpoint_answer(&two, &by_label, EndpointKind::Speakers).unwrap_err();
        assert!(code == 2 && why.starts_with("error: more than one cable is labelled B"), "{why}");
    }

    #[test]
    fn wrong_command_lines_say_why() {
        for (line, why) in [
            ("bogus", "no command 'bogus'"),
            ("add", "'add' takes a cable's label"),
            ("add B_1", "isn't a cable label"),
            ("add B --mics two", "--mics takes both, call or low-latency, not 'two'"),
            ("add B --mics", "--mics takes both"),
            ("add B C", "'add' takes 1 argument; 'C' is one more"),
            ("add B --forget", "'add' doesn't take that option"),
            ("remove", "'remove' takes a cable's label, or --all"),
            ("remove B --all", "not both"),
            ("variant B", "'variant' takes a label and both"),
            ("variant B two", "not 'two'"),
            ("update", "'update' takes the package's folder"),
            ("list --all", "'list' doesn't take that option"),
            ("list --x", "'list' doesn't take --x"),
            ("migrate C:\\pkg --forget", "'migrate' doesn't take that option"),
            ("migrate C:\\pkg --endpoint-map", "--endpoint-map takes a file"),
            ("update C:\\pkg --adopt previous", "'update' doesn't take that option"),
            ("remove B --endpoint-map m.json", "'remove' doesn't take that option"),
            ("disable", "'disable' takes a cable's label"),
            ("enable", "'enable' takes a cable's label"),
            ("disable B C", "'disable' takes 1 argument; 'C' is one more"),
            ("enable B --forget", "'enable' doesn't take that option"),
            ("disable B --mics call", "'disable' doesn't take that option"),
            ("endpoint", "'endpoint' takes a cable's label or id"),
            ("endpoint B", "'endpoint' takes a role after the cable"),
            ("endpoint B mic", "'endpoint' takes In, Out or Out-5ms, not 'mic'"),
            ("endpoint B_1 speakers", "isn't a cable label"),
            ("endpoint B speakers x", "'endpoint' takes 2 arguments; 'x' is one more"),
        ] {
            let e = p(line).unwrap_err();
            assert!(e.contains(why), "{line}: {e}");
        }
    }
}
