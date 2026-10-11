# The Windows installer

`findery.wxs` — a WiX v4+ source — is the whole definition. It is what
`.github/workflows/release.yml` builds, and it can be built by hand.

```powershell
dotnet tool install --global wix --version 5.0.2
wix extension add --global WixToolset.Util.wixext/5.0.2

cargo build --release -p findery-cli -p findery-server -p findery-mcp
New-Item -ItemType Directory -Force artifacts | Out-Null
foreach ($b in @("findery", "finderyd", "findery-mcp")) {
  Copy-Item "target/release/$b.exe" "artifacts/$b.exe"
}

wix build -arch x64 -ext WixToolset.Util.wixext -d Version=0.1.0 `
          packaging/windows/findery.wxs -o artifacts/findery-0.1.0-x64.msi
```

The manifest reads `artifacts/` and `README.md`, and nothing else — so it can be
built without knowing how the binaries were produced. Run it from the repository
root.

## What it installs

```
C:\Program Files\findery\findery.exe
C:\Program Files\findery\finderyd.exe
C:\Program Files\findery\findery-mcp.exe
C:\Program Files\findery\README.md
```

Plus `C:\Program Files\findery` appended to the **system** `PATH`, and
`HKLM\Software\findery\InstallDir` as a breadcrumb.

## The decisions worth knowing

**All three binaries in one directory.** Not tidiness: `findery` looks for
`finderyd` next to its own executable before it looks on `PATH`, so that a CLI
can never end up talking to a server from a different build. An installer that
filed them apart would break that.

**`PATH` is modified, machine-wide.** This tool is typed as `findery` from inside
whatever project directory you are in — that is the entire product — so an
installer that left it off `PATH` would ship a command nobody could run. The
entry is written by the component that owns `findery.exe`, so uninstalling
removes exactly what was added and nothing else does.

**`util:CloseApplication` on `finderyd.exe`.** The daemon starts itself on the
first `findery` command and stays up, and Windows will not replace a running
executable. Without this, every upgrade lands on a locked file — the ordinary
case here, not an edge one, because the daemon is *designed* to be running and
the user has no reason to know it exists. The element has the Restart Manager
close the process before the files are replaced.

**WiX v5, pinned, and not v6 or later.** From v6 the toolset requires accepting
the Open Source Maintenance Fee EULA. That is a licensing decision for the
project to make, not one a workflow should accept by default.

**GUIDs are stable for life.** `UpgradeCode` is what lets a newer version replace
an older one in place; changing it makes them unrelated products that install
side by side. Each component's `Guid` is its identity to Windows Installer —
changing one makes the component look new, and its files get duplicated rather
than replaced. Neither may be regenerated.

**The version is the tag.** The workflow refuses to build when the tag and
`Cargo.toml` disagree, because otherwise the installer is labelled one version
and contains another, and that is only noticed after it is published. A tag must
be `v<major>.<minor>.<patch>` — MSI versions are numeric and at most four parts.
Non-tag runs build `0.0.0`, so the packaging path is exercised before a release
depends on it.

## Not shipped

**A `LICENSE` file.** `Cargo.toml` declares `MIT OR Apache-2.0` and no licence
text exists in the repository, so there is nothing for the installer to carry.
An installer that redistributes binaries should ship its terms; adding the file
is a decision about which licence and whose copyright line, and it has not been
made.
