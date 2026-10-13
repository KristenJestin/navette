# Installing Navette

No root, no build: a release or the beta is an archive of the Releases page of this repository
(`navette-<version>-linux-x86_64.tar.gz`, with its `.sha256`), built on Ubuntu 24.04 for Linux
x86_64. It needs glibc 2.39 or newer (Omarchy has it). `<owner>` below is the owner of this
repository.

- **A release** (`vX.Y.Z`) is made when `dev` is merged into `main`.
- **The beta** (`vX.Y.Z-beta.N`, marked pre-release) is made at every push to `dev`, and only the
  newest one exists. It is the patch after the last release, so it is newer than it.

Install the same version on both machines: see [what a version promises](versions.md).

## Install

On each machine, in an empty folder, with `<tag>` the release (`v0.1.0`) or the beta (`v0.1.1-beta.21`)
shown on the Releases page:

```
gh release download <tag> -R <owner>/navette --pattern 'navette-*'
sha256sum --check navette-*.sha256
tar -xzf navette-*.tar.gz
cd navette-*-linux-x86_64
install -Dm755 navette ~/.local/bin/navette
navette --version
```

`~/.local/bin` must be in your `PATH`. Without `gh`, download the two files from the Releases page
in a browser and run the same commands from `sha256sum` on.

On the **remote machine** (the work laptop) only, once `navette pair <home:port>` has succeeded
(see [the link](link.md)), install the agent's unit, a systemd user service:

```
install -Dm644 navette-agent.service ~/.config/systemd/user/navette-agent.service
systemctl --user daemon-reload
systemctl --user enable --now navette-agent
```

Its logs: `journalctl --user -u navette-agent`.

## Update

Download the newer archive the same way and install the binary over the old one. At home,
restart whatever runs `navette serve`. On the remote machine:

```
systemctl --user restart navette-agent
```

The config in `~/.config/navette` stays: no new pairing is needed within a minor version.

## Remove everything

On the remote machine:

```
systemctl --user disable --now navette-agent
rm ~/.config/systemd/user/navette-agent.service
systemctl --user daemon-reload
```

On both machines:

```
rm ~/.local/bin/navette
rm -r ~/.config/navette     # the identity and the pairing; pair again to use Navette later
```

(If you moved the config with `NAVETTE_CONFIG_DIR`, remove that folder instead.)
