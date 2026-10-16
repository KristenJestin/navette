# What a version promises

Nobody edits a version: it comes from the Angular subjects of the pull requests merged into
`main` (`feat` moves the minor in `0.x`, `fix` the patch, a breaking change also the minor), and
the binary reports it with `navette --version`. A local build says `0.0.0-dev`.

Navette is in `0.x`: a 1.0 is the maintainer's decision. From 1.0, a breaking change makes a new
major version (semantic-release counts it from the `BREAKING CHANGE:` footer of a commit). Until
then it moves the minor, and the promises below hold **within a minor version** (`0.3.x`).

A change is breaking when it breaks one of four promises.

## The link between the two machines

Two Navette of the same minor version talk to each other: pairing, the TLS link and the messages
over it. That is the rule the link enforces:

- **The same minor links.** The link opens with a hello carrying each machine's version
  (`navette --version`); 0.2.0 links with 0.2.3, and each side shows the other's version
  (`peer_version` in [the events](events.md)).
- **Another minor is refused**, on both sides: each logs once which machine runs what and which
  to update, its events say `incompatible` with `peer_version`, and `navette status` on the desk
  says it. The agent then dials again once a minute, on the desk's network only, and links as
  soon as the older machine is updated. A 0.1.x, whose hello carries no version, is refused as
  `0.1`.
- A beta (`vX.Y.Z-beta.N`) counts as its minor. It is the patch after the last release and
  promises nothing beyond what that release did, until it is released.
- A local build (`0.0.0-dev`) links only with another local build.
- **A change to what the link carries is a `feat`** (a new minor version), never a `fix`: a
  patch must link with every patch of its minor. A message the other side ignores is the only
  change a `fix` may make (see `src/wire.rs`).

The link has no version of its own any more: before 0.2 its hello carried a hand-bumped link
version, which nobody had to bump for 0.1.0 and 0.2.0 to talk past each other. Navette's version
replaces it, and moves by itself.

Install the same minor version on both machines.

Breaking: a change that makes a Navette unable to pair with, or to serve, a Navette of the same
minor.
Not breaking: a new message the other side may ignore.

## The command line

`navette`, its commands (`pair`, `serve`, `agent`...), their flags and environment variables
(`NAVETTE_CONFIG_DIR`).

Breaking: a command or a flag removed or renamed, or one that no longer does what it did.
Not breaking: a new command, a new optional flag.

## The events

The lines of `events.sock` and `navette watch`, and the commands on that socket, are a contract
of their own, with its version `v` on every line ([the events](events.md)).

Breaking: what changes `v` there (a field or a value removed, renamed or changing meaning).
Not breaking: a new field, a new value, a new command.

## The config files

What Navette keeps in its config directory (`identity.key`, `identity.crt`, `paired.conf`, and
what the modes will add). An update keeps them and reads them: no new pairing within a minor.

Breaking: a change that makes the maintainer pair again or edit a file by hand.
Not breaking: a file that Navette migrates by itself and keeps the meaning of.
