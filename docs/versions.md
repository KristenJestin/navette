# What a version promises

Nobody edits a version: it comes from the Angular subjects of the pull requests merged into
`main` (`feat` moves the minor in `0.x`, `fix` the patch, a breaking change also the minor), and
the binary reports it with `navette --version`. A local build says `0.0.0-dev`.

Navette is in `0.x`: a 1.0 is the maintainer's decision. From 1.0, a breaking change makes a new
major version (semantic-release counts it from the `BREAKING CHANGE:` footer of a commit). Until
then it moves the minor, and the promises below hold **within a minor version** (`0.3.x`).

A change is breaking when it breaks one of three promises.

## The link between the two machines

Two Navette of the same minor version talk to each other: pairing, the TLS link and the messages
over it. Install the same version on both machines. A beta (`vX.Y.Z-beta.N`) is the patch after
the last release and promises nothing beyond what that release did, until it is released.

Breaking: a change that makes a Navette unable to pair with, or to serve, a Navette of the same
minor.
Not breaking: a new message the other side may ignore.

## The command line

`navette`, its commands (`pair`, `serve`, `agent`...), their flags and environment variables
(`NAVETTE_CONFIG_DIR`).

Breaking: a command or a flag removed or renamed, or one that no longer does what it did.
Not breaking: a new command, a new optional flag.

## The config files

What Navette keeps in its config directory (`identity.key`, `identity.crt`, `paired.conf`, and
what the modes will add). An update keeps them and reads them: no new pairing within a minor.

Breaking: a change that makes the maintainer pair again or edit a file by hand.
Not breaking: a file that Navette migrates by itself and keeps the meaning of.
