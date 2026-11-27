# Cargo Ark CLI

Anvil Release Kit (ARK) provides tools for Rust release workflows through `cargo ark`.

## Release dependency validation

`cargo ark release-dependencies` checks whether the selected packages build and
pass their shipped tests from their `.crate` archives. Run it from the workspace
and pass the candidates as a JSON array on standard input:

```sh
echo '[{"name":"my-crate","new_version":"1.2.0"}]' | cargo ark release-dependencies
```

The command runs each package's tests with all features, with no default
features, and without feature flags (Cargo's default selection).
It does not discover candidates or publish packages.
