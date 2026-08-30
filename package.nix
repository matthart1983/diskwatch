{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "diskwatch";
  # Keep in sync with Cargo.toml's `version` on every release — the Nix
  # derivation label otherwise drifts from the actual source contents.
  version = "0.5.0";

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Single-host, read-only disk diagnostics TUI — sibling to netwatch and syswatch";
    homepage = "https://github.com/matthart1983/diskwatch";
    license = lib.licenses.mit;
    mainProgram = "diskwatch";
  };
}
