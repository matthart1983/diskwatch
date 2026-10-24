{
  lib,
  rustPlatform,
  versionCheckHook,
  # Short git rev of the source, appended to the version as semver build
  # metadata. The flake passes it; null leaves the plain Cargo.toml version.
  rev ? null,
}:

let
  cargoToml = lib.importTOML ./Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version + lib.optionalString (rev != null) "+${rev}";

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;
  # `diskwatch --version` prints only the Cargo.toml version, not the rev.
  preVersionCheck = ''
    version=${cargoToml.package.version}
  '';

  meta = {
    description = "Single-host, read-only disk diagnostics TUI — sibling to netwatch and syswatch";
    homepage = "https://github.com/matthart1983/diskwatch";
    license = lib.licenses.mit;
    mainProgram = "diskwatch";
  };
}
