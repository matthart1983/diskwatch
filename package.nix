{
  lib,
  rustPlatform,
  versionCheckHook,
}:

let
  cargoToml = lib.importTOML ./Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit (cargoToml.package) version;

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;

  meta = {
    description = "Single-host, read-only disk diagnostics TUI — sibling to netwatch and syswatch";
    homepage = "https://github.com/matthart1983/diskwatch";
    license = lib.licenses.mit;
    mainProgram = "diskwatch";
  };
}
