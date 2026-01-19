{
  description = "A flake for talos-pilot, a Talos TUI for real-time node monitoring.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default";
  };

  outputs =
    {
      self,
      nixpkgs,
      systems,
    }:
    let
      inherit (nixpkgs) lib;
      forEachPkgs = f: lib.genAttrs (import systems) (system: f nixpkgs.legacyPackages.${system});

      package =
        {
          lib,
          rustPlatform,
          protobuf,
        }:
        let
          manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        in
        rustPlatform.buildRustPackage rec {
          inherit (manifest.workspace.package) version;

          pname = "talos-pilot";
          src = ./.;

          cargoDeps = rustPlatform.importCargoLock {
            lockFile = src + "/Cargo.lock";
          };

          nativeBuildInputs = [
            protobuf
          ];

          meta = {
            description = "Talos TUI for real-time node monitoring, log streaming, etcd health, and diagnostics";
            homepage = "https://github.com/Handfish/talos-pilot";
            license = with lib.licenses; [ mit ];
            mainProgram = "talos-pilot";
          };
        };
    in
    {
      packages = forEachPkgs (pkgs: rec {
        talos-pilot = pkgs.callPackage package { inherit (pkgs) protobuf; };
        default = talos-pilot;
      });
      devShells = forEachPkgs (pkgs: {
        default = pkgs.mkShell {
          # automatically pulls nativeBuildInputs + buildInputs
          inputsFrom = [ (pkgs.callPackage package { inherit (pkgs) protobuf; }) ];
        };
      });
      overlays.default = final: _: {
        talos-pilot = final.callPackage package { };
      };
    };
}
