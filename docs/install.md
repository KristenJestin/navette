# Installing Navette

No root, no build: a release or the beta is an archive of the Releases page of this repository
(`navette-<version>-linux-x86_64.tar.gz`, with its `.sha256`), built on Ubuntu 24.04 for Linux
x86_64. It needs glibc 2.39 or newer (Omarchy has it).

- **A release** (`vX.Y.Z`) is made when `dev` is merged into `main`.
- **The beta** (`vX.Y.Z-beta.N`, marked pre-release) is made at every push to `dev`, and only the
  newest one exists. It is the patch after the last release, so it is newer than it.

Install the same version on both machines: see [what a version promises](versions.md).

## Install

On each machine, in an empty folder, with `<tag>` the release (`v0.1.0`) or the beta (`v0.1.1-beta.21`)
shown on the Releases page:

```
gh release download <tag> -R KristenJestin/navette --pattern 'navette-*'
sha256sum --check navette-*.sha256
tar -xzf navette-*.tar.gz
cd navette-*-linux-x86_64
./install.sh
navette --version
```

`install.sh` puts `navette` in `~/.local/bin`, the icon Navette's notifications show in
`~/.local/share/icons/hicolor/scalable/apps`, and the units in `~/.config/systemd/user`, then runs
`systemctl --user daemon-reload`. It refuses to run as root, enables nothing and starts nothing,
and running it again (to update, say) changes nothing more. `~/.local/bin` must be in your
`PATH`. Without `gh`, download the two files from the Releases page in a browser and run the same
commands from `sha256sum` on.

Then, once `navette pair` has succeeded on the machine (see [the link](link.md)), the same unit
on both machines runs what the pairing says: `navette serve --audio` at home, `navette agent` on
the remote machine.

```
systemctl --user enable --now navette
```

Its logs: `journalctl --user -u navette`. It is tied to the graphical session (Navette reaches
Hyprland and Wayland), so it starts at login and stops at logout. If this machine is not paired,
`navette run` exits with status 78 and `navette: not paired; run navette pair first`, and the
unit does not try again until you start it.

`navette-agent.service` is still shipped for one minor version, and `navette.service` replaces
it: on a remote machine that has it, run
`systemctl --user disable --now navette-agent` and then enable `navette`.

### By hand

The steps of `install.sh`, from the folder of the archive:

```
install -Dm755 navette ~/.local/bin/navette
install -Dm644 navette.svg ~/.local/share/icons/hicolor/scalable/apps/navette.svg
install -Dm644 navette.service ~/.config/systemd/user/navette.service
systemctl --user daemon-reload
```

## Update

Download the newer archive the same way and run its `install.sh` over the old install. Then, on
each machine:

```
systemctl --user restart navette
```

The config in `~/.config/navette` stays: no new pairing is needed within a minor version.
An agent paired before it recorded the desk's router learns it at its next link (see
[the link](link.md#only-from-the-desks-network)).

## Remove everything

On both machines:

```
systemctl --user disable --now navette
rm ~/.config/systemd/user/navette.service
systemctl --user daemon-reload
```

On a machine that still has the old unit, also `systemctl --user disable --now navette-agent` and
`rm ~/.config/systemd/user/navette-agent.service`. Then:

```
rm ~/.local/bin/navette
rm ~/.local/share/icons/hicolor/scalable/apps/navette.svg
rm -r ~/.config/navette     # the identity and the pairing; pair again to use Navette later
```

(If you moved the config with `NAVETTE_CONFIG_DIR`, remove that folder instead.)
