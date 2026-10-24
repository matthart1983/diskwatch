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
        diskwatch = pkgs.callPackage ./package.nix {
          rev = self.shortRev or self.dirtyShortRev or null;
        };
      in
      {
        packages = {
          diskwatch = diskwatch;
          default = diskwatch;
        };

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
