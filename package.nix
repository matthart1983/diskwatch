{
  lib,
  rustPlatform,
  versionCheckHook,
  # Appended to the Cargo.toml version as semver build metadata, e.g.
  # `+git.20261024.aad375a`. The flake passes it; empty leaves the plain
  # Cargo.toml version.
  versionSuffix ? "",
}:

let
  cargoToml = lib.importTOML ./Cargo.toml;
  version = cargoToml.package.version + versionSuffix;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit version;

  # Only what cargo reads, so a docs or CI change doesn't rebuild.
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  # `diskwatch --version` reports this, so versionCheckHook checks the full
  # version, rev included.
  DISKWATCH_BUILD_VERSION = version;

  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;

  meta = {
    description = "Single-host, read-only disk diagnostics TUI — sibling to netwatch and syswatch";
    homepage = "https://github.com/matthart1983/diskwatch";
    license = lib.licenses.mit;
    mainProgram = "diskwatch";
  };
}
