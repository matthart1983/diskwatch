{
  description = "diskwatch — single-host disk diagnostics TUI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        rev = self.shortRev or self.dirtyShortRev or null;
        diskwatch = pkgs.callPackage ./package.nix {
          # The commit date comes before the rev so that builds of one Cargo
          # version sort by date in Nix's version ordering, not by hex digits.
          versionSuffix = pkgs.lib.optionalString (rev != null)
            "+git.${builtins.substring 0 8 self.lastModifiedDate}.${rev}";
        };
      in
      {
        packages = {
          diskwatch = diskwatch;
          default = diskwatch;
        };

        # `nix flake check` builds the package, which runs cargo test in the
        # sandbox and then versionCheckHook.
        checks.diskwatch = diskwatch;

        devShells.default = pkgs.mkShell {
          inputsFrom = [ diskwatch ];
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      }
    );
}
