# egui-plugin-window

An egui editor window inside a plug-in host's view on macOS, drawn with wgpu.
It is the windowing code under the editors of
[harmonigraph](https://github.com/yan-h/harmonigraph) and otonal, two CLAP
plug-ins that load into the same Bitwig Studio process, kept in one place so
that a macOS windowing fix is written once.

It is not a general library. Nothing here is published to crates.io, no
platform but macOS opens a window, and no host but Bitwig is tested.

## What is here

- `baseview/`: the AppKit layer. A view parented into the host's, its frames
  from the view's display link, and its mouse, key and window events.
- `egui-baseview/`: egui on that view. It decides whether a frame runs the UI,
  translates input, and renders with wgpu.
- `plugin-own-window/`: the editor's own window. It moves the editor's view
  out of the host's window into one of its own, so the editor stays on
  screen while another app is in front, and back, and keeps the editor's
  size: what the host is told, what the project saves, and when the own
  window has the say. A shell sees it through `EditorSize` alone. It
  depends on AppKit alone, through the objc2 crates, not on the other two.

The first two began as crates.io releases (`baseview` 0.1.4, `egui-baseview` 0.3.0,
MIT OR Apache-2.0, licenses in each directory) and are no longer reconciled
with upstream. They keep those crate names so that a project's
`[patch.crates-io]` selects them. `PATCHES.md` records what each changes from
its release, and where `plugin-own-window`, which has no upstream, came from.
Off macOS, `baseview` and `plugin-own-window` are stubs that open no window,
so a project's headless tests build on Linux.

## Using it from a project

Pin one commit in the project's root `Cargo.toml`. `egui-baseview` names
`baseview` by path, so it brings the `baseview` from the same commit:

```toml
[patch.crates-io]
egui-baseview = { git = "https://github.com/yan-h/egui-plugin-window", rev = "<commit>" }
```

`egui-baseview` re-exports `baseview` as `egui_baseview::baseview`, which is
how a project should reach it. A project that also names `baseview` itself,
directly or through another crate, patches it at the same commit:

```toml
baseview = { git = "https://github.com/yan-h/egui-plugin-window", rev = "<commit>" }
```

At another commit it builds a second `baseview`, whose types do not meet
`egui-baseview`'s.

`plugin-own-window` is no crates.io crate, so it is a plain git dependency of
the crate that opens the editor, at the same commit:

```toml
[dependencies]
plugin-own-window = { git = "https://github.com/yan-h/egui-plugin-window", rev = "<commit>" }
```

A change here reaches a project when that project moves its pins, in a pull
request of its own.

## Tests

```
./ci.sh
```

On macOS it runs the three crates' tests; elsewhere it checks that they
build. `plugin-own-window` is also held to rustfmt and clippy.
The `frame-log` feature of `egui-baseview` builds only against
harmonigraph's patched wgpu-hal, so harmonigraph's CI builds it, not this one.
