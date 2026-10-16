//! `navette run`: which command this machine's pairing says to run (`serve` at home, `agent` on
//! the remote machine), so that one systemd unit serves both machines.

use std::fmt;
use std::io;

use crate::store::{Role, Store};

/// The exit status of a refusal (`EX_CONFIG`): the unit lists it in `RestartPreventExitStatus`,
/// so that a machine with no usable pairing does not restart in a loop.
pub const REFUSED: u8 = 78;

/// What the pairing says to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Home: `navette serve`.
    Serve { audio: bool },
    /// Remote: `navette agent`. `audio_ignored` is set when `--audio` was given, which only
    /// home uses.
    Agent { audio_ignored: bool },
}

/// `navette run` cannot tell what to run: no pairing, or one it cannot read.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused(String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

impl From<Refused> for io::Error {
    fn from(refused: Refused) -> Self {
        io::Error::other(refused)
    }
}

/// What to run, from the pairing in `store`.
pub fn plan(store: &Store, audio: bool) -> Result<Plan, Refused> {
    let pairing = store
        .pairing()
        // A file that does not read does not heal by itself: restarting would only spin.
        .map_err(|e| Refused(e.to_string()))?
        .ok_or_else(|| Refused("not paired; run navette pair first".into()))?;
    // No wildcard: a role added later must say here what it runs.
    Ok(match pairing.role {
        Role::Home => Plan::Serve { audio },
        Role::Remote => Plan::Agent {
            audio_ignored: audio,
        },
    })
}

/// The exit status for `error`: [`REFUSED`] for a refusal, 1 for anything else.
pub fn exit_code(error: &io::Error) -> u8 {
    match error.get_ref() {
        Some(inner) if inner.is::<Refused>() => REFUSED,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::identity::Identity;
    use crate::store::{Pairing, Role};

    fn paired_as(role: Role) -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        store
            .save_pairing(&Pairing {
                role,
                peer: Identity::generate().unwrap().fingerprint(),
                address: "192.0.2.10:7420".parse().unwrap(),
                gateway: None,
            })
            .unwrap();
        (tmp, store)
    }

    #[test]
    fn a_home_pairing_runs_serve_with_audio() {
        let (_tmp, store) = paired_as(Role::Home);
        assert_eq!(plan(&store, true), Ok(Plan::Serve { audio: true }));
    }

    #[test]
    fn a_home_pairing_runs_serve_without_audio_when_not_asked() {
        let (_tmp, store) = paired_as(Role::Home);
        assert_eq!(plan(&store, false), Ok(Plan::Serve { audio: false }));
    }

    #[test]
    fn a_remote_pairing_runs_the_agent() {
        let (_tmp, store) = paired_as(Role::Remote);
        assert_eq!(
            plan(&store, false),
            Ok(Plan::Agent {
                audio_ignored: false
            })
        );
    }

    #[test]
    fn a_remote_pairing_ignores_audio_and_says_so() {
        let (_tmp, store) = paired_as(Role::Remote);
        assert_eq!(
            plan(&store, true),
            Ok(Plan::Agent {
                audio_ignored: true
            })
        );
    }

    #[test]
    fn no_pairing_is_refused_with_the_way_out() {
        let tmp = tempfile::tempdir().unwrap();
        let refused = plan(&Store::new(tmp.path()), true).unwrap_err();
        assert_eq!(refused.to_string(), "not paired; run navette pair first");
    }

    #[test]
    fn a_broken_role_names_the_key_and_the_file() {
        let (tmp, store) = paired_as(Role::Home);
        let file = tmp.path().join("paired.conf");
        let text = fs::read_to_string(&file).unwrap();
        fs::write(&file, text.replace("role = desk", "role = boss")).unwrap();
        let refused = plan(&store, false).unwrap_err().to_string();
        assert!(refused.contains("role"), "{refused}");
        assert!(refused.contains("boss"), "{refused}");
        assert!(refused.contains(file.to_str().unwrap()), "{refused}");
    }

    #[test]
    fn a_missing_role_names_the_key_and_the_file() {
        let (tmp, store) = paired_as(Role::Home);
        let file = tmp.path().join("paired.conf");
        let text = fs::read_to_string(&file).unwrap();
        fs::write(&file, text.replace("role = desk\n", "")).unwrap();
        let refused = plan(&store, false).unwrap_err().to_string();
        assert!(refused.contains("role"), "{refused}");
        assert!(refused.contains(file.to_str().unwrap()), "{refused}");
    }

    #[test]
    fn a_refusal_exits_with_the_status_the_unit_does_not_restart_on() {
        let refused = io::Error::other(Refused("not paired".into()));
        assert_eq!(exit_code(&refused), REFUSED);
        assert_eq!(exit_code(&io::Error::other("the link broke")), 1);
    }
}
