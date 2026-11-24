//! acx-ll-cable: the cables' setup tool (TODO 19, the per-cable design, section 9). A thin command line over nf-vmic's
//! `setup` and `variant`; no audio, no app, no broker, so a user of plain cables needs nothing else. `list` is for any user;
//! the rest needs an elevated administrator (the setup's lock refuses anyone else before anything is read).
//!
//! Exit codes: 0 done (or nothing to do), 1 usage, 2 an error (the message says what changed, if anything), 7 Windows needs a
//! restart to finish.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::path::PathBuf;
use std::process::ExitCode;

use nf_vmic::identity::Label;
use nf_vmic::setup::plan::RemoveTarget;
use nf_vmic::setup::{self, Done, SetupError};
use nf_vmic::variant::{self, Outcome, Variant};

const USAGE: &str = "\
acx-ll-cable: ACX-LL-Cable's setup (any number of cables, each its own Speakers and microphones)

  acx-ll-cable list
      The present cables, their devices and endpoints, the remembered absent ones, and setup errors. Any user.
  acx-ll-cable add <label> [--mics both|call|low-latency] [--adopt <instance ID>]
      Makes cable <label> (one to eight of A-Z and 0-9), on the driver package the other cables run. Present and working: no
      change. Present but not working: repaired in place. Removed earlier: back with its identity and endpoints. --mics: its
      microphones (default both, or what it had). --adopt: re-creates a device made outside this tool as the cable.
  acx-ll-cable remove <label> [--forget]
  acx-ll-cable remove --all [--forget]
      Removes the cable's device (every device of ours with --all, the old driver's too). Its identity stays, so `add` brings
      it back with its endpoints, unless --forget.
  acx-ll-cable variant <label> both|call|low-latency
      Switches the cable's microphones: its device re-created with new endpoints; no other cable is touched.
  acx-ll-cable update <package folder>
      Moves every cable to the package in the folder (driver\\out\\Release\\AudioCodec): every cable stops for a few seconds.
      If the old driver image doesn't unload (an app has a cable's control device open), every cable is put back as it was.
  acx-ll-cable migrate <package folder> [--a-mics m] [--b-mics m]
      From the old driver's devices (ROOT\\AudioCodec as A, ROOT\\NoiseFilterCableB as B) to cables on the package in the
      folder. A keeps its variant unless --a-mics; B gets --b-mics (default both). Nothing changes unless the package makes
      both; A and B are recorded first, so a run that stops part way is finished by running this again.

Exit codes: 0 done, 1 usage, 2 an error, 7 Windows needs a restart to finish.";

/// A parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cmd {
    Help,
    List,
    Add { label: Label, mics: Option<Variant>, adopt: Option<String> },
    Remove { target: RemoveTarget, forget: bool },
    Variant { label: Label, target: Variant },
    Update { folder: PathBuf },
    Migrate { folder: PathBuf, a_mics: Option<Variant>, b_mics: Option<Variant> },
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
    while let Some(a) = flags.next() {
        match a.as_str() {
            "--mics" => opt_mics = Some(mics(a, flags.next())?),
            "--a-mics" => a_mics = Some(mics(a, flags.next())?),
            "--b-mics" => b_mics = Some(mics(a, flags.next())?),
            "--adopt" => adopt = Some(flags.next().ok_or("--adopt takes a device's instance ID")?.clone()),
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
        "add" => Cmd::Add { label: label(positional.first().copied(), "add")?, mics: opt_mics, adopt: adopt.clone() },
        "remove" => {
            let target = match (all, positional.first()) {
                (true, None) => RemoveTarget::All,
                (false, Some(l)) => RemoveTarget::Label(label(Some(l), "remove")?),
                (true, Some(_)) => return Err("'remove' takes a label or --all, not both".into()),
                (false, None) => return Err("'remove' takes a cable's label, or --all".into()),
            };
            Cmd::Remove { target, forget }
        }
        "variant" => {
            let target = positional.get(1).ok_or("'variant' takes a label and both, call or low-latency")?;
            let target = Variant::from_arg(target).ok_or_else(|| format!("'variant' takes both, call or low-latency, not '{target}'"))?;
            Cmd::Variant { label: label(positional.first().copied(), "variant")?, target }
        }
        "update" => Cmd::Update { folder: positional.first().ok_or("'update' takes the package's folder")?.into() },
        "migrate" => Cmd::Migrate { folder: positional.first().ok_or("'migrate' takes the package's folder")?.into(), a_mics, b_mics },
        other => return Err(format!("no command '{other}'")),
    };
    // What each command takes, and how many positional arguments
    let (max, ok) = match &cmd {
        Cmd::Help | Cmd::List => (0, allowed(&[opt_mics.is_none(), adopt.is_none(), !forget, !all, a_mics.is_none(), b_mics.is_none()])),
        Cmd::Add { .. } => (1, allowed(&[!forget, !all, a_mics.is_none(), b_mics.is_none()])),
        Cmd::Remove { .. } => (1, allowed(&[opt_mics.is_none(), adopt.is_none(), a_mics.is_none(), b_mics.is_none()])),
        Cmd::Variant { .. } => (2, allowed(&[opt_mics.is_none(), adopt.is_none(), !forget, !all, a_mics.is_none(), b_mics.is_none()])),
        Cmd::Update { .. } => (1, allowed(&[opt_mics.is_none(), adopt.is_none(), !forget, !all, a_mics.is_none(), b_mics.is_none()])),
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

fn exit_of(r: Result<Done, SetupError>) -> ExitCode {
    match r {
        Ok(Done::Nothing) => {
            say("Nothing to do.");
            ExitCode::SUCCESS
        }
        Ok(Done::Changed) => {
            say("Done.");
            ExitCode::SUCCESS
        }
        Ok(Done::RestartPending { device }) => {
            say(&format!("Windows needs a restart to finish with {device}: restart, then run `acx-ll-cable list`."));
            ExitCode::from(7)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
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
        Cmd::Add { label, mics, adopt } => exit_of(setup::add(&label, mics, adopt.as_deref(), &mut out)),
        Cmd::Remove { target, forget } => exit_of(setup::remove(&target, forget, &mut out)),
        Cmd::Variant { label, target } => exit_of(variant::switch(&label, target, &mut out).map(|o| match o {
            Outcome::Nothing => Done::Nothing,
            Outcome::Switched => Done::Changed,
            Outcome::RestartPending { device } => Done::RestartPending { device },
        })),
        Cmd::Update { folder } => exit_of(setup::update(&folder, &mut out)),
        Cmd::Migrate { folder, a_mics, b_mics } => exit_of(setup::migrate(&folder, a_mics, b_mics, &mut out)),
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
        assert_eq!(p("add d"), Ok(Cmd::Add { label: l("D"), mics: None, adopt: None }));
        assert_eq!(p("add B --mics call"), Ok(Cmd::Add { label: l("B"), mics: Some(Variant::Call), adopt: None }));
        assert_eq!(p(r"add C --adopt ROOT\MEDIA\0007"), Ok(Cmd::Add { label: l("C"), mics: None, adopt: Some(r"ROOT\MEDIA\0007".into()) }));
        assert_eq!(p("remove B"), Ok(Cmd::Remove { target: RemoveTarget::Label(l("B")), forget: false }));
        assert_eq!(p("remove B --forget"), Ok(Cmd::Remove { target: RemoveTarget::Label(l("B")), forget: true }));
        assert_eq!(p("remove --all --forget"), Ok(Cmd::Remove { target: RemoveTarget::All, forget: true }));
        assert_eq!(p("variant B low-latency"), Ok(Cmd::Variant { label: l("B"), target: Variant::LowLatency }));
        assert_eq!(p(r"update C:\pkg"), Ok(Cmd::Update { folder: r"C:\pkg".into() }));
        assert_eq!(p(r"migrate C:\pkg"), Ok(Cmd::Migrate { folder: r"C:\pkg".into(), a_mics: None, b_mics: None }));
        assert_eq!(
            p(r"migrate C:\pkg --a-mics call --b-mics low-latency"),
            Ok(Cmd::Migrate { folder: r"C:\pkg".into(), a_mics: Some(Variant::Call), b_mics: Some(Variant::LowLatency) })
        );
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
        ] {
            let e = p(line).unwrap_err();
            assert!(e.contains(why), "{line}: {e}");
        }
    }
}
