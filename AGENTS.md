# AGENTS.md

The egui editor window shared by harmonigraph and otonal: `baseview` (AppKit)
and `egui-baseview` (egui and wgpu on it), as `README.md` describes.
`CLAUDE.md` is a symlink to this file.

## Stakes record (a draft for Yan to correct)

Inferred from the two projects' records, which stay the source: read the
stakes record in each project's contract before pricing a case here.

- **Who and where:** Yan alone, in both plug-ins' editors, in Bitwig Studio on
  macOS on Apple silicon with a 144 Hz display. Linux is only where cloud
  sessions build and test, against the no-window stub.
- **Severe:** a panic or an Objective-C exception anywhere in this code, which
  runs in Bitwig's plug-in host process and takes it down with every plug-in
  in it; time taken from Bitwig's main thread, which every frame here runs on;
  memory that grows while an editor is open.
- **Cheap:** a frame drawn late, a glitch for a frame, a cursor that is wrong
  until the next move.
- **Unsupported, unprotected:** other hosts, other platforms opening a
  window, OpenGL.

## Every change is felt in two plug-ins

A change here reaches a plug-in only when that project moves its pin. The
pull request here says which project asked for the change and what each
plug-in's player would see once pinned; a change either player would notice
is Yan's to decide, as in the projects.

## Working here

- A session that changes tracked files works in a worktree under
  `.claude/worktrees/<name>/`, never in the main checkout, and ends in a
  draft pull request. Nothing merges unless Yan asks.
- `./ci.sh` green is the definition of done. Neither crate is checked with
  rustfmt or clippy yet: the two carry different formatting from upstream.
- To try a change in a project before pushing it, build that project with
  both crates pointed at this checkout:

  ```
  cargo build -p <plugin> \
    --config 'patch.crates-io.baseview.path="<this checkout>/baseview"' \
    --config 'patch.crates-io.egui-baseview.path="<this checkout>/egui-baseview"'
  ```
