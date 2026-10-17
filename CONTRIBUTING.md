# Contributing

Thanks for wanting to help! Kludd is small on purpose, so a quick note
before you start:

- **Open an issue first** for anything bigger than a bug fix, so we can agree
  on it before you spend time on it.
- **The look is fixed.** `design/design-reference.html` is the spec. Visual
  changes need a matching change there and before/after screenshots.
- **Notes must never be mangled.** Any change to `src/markdown.rs` needs a
  round-trip test: parse → serialize → the identical file.
- Before sending a pull request:

  ```sh
  cargo fmt
  cargo clippy --all-targets -- -D warnings
  cargo test
  ```

  The editor test drives real GTK widgets; it runs when a display is
  available and is skipped otherwise (as in CI).

To try a build next to the Kludd you use every day, give it its own app id,
so the two don't talk to each other:

```sh
KLUDD_APP_ID=io.github.loveranmar.KluddTest cargo run
```

Keep dependencies minimal: new crates need a good reason.
