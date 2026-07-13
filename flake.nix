{
  description = "Run a coding agent in a sandboxed environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    fenix,
  }: let
    systems = [
      "aarch64-linux"
      "x86_64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs systems;
    version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

    mkAgentRun = pkgs: let
      toolchain = fenix.packages.${pkgs.stdenv.hostPlatform.system}.minimal.toolchain;
      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };
    in
      rustPlatform.buildRustPackage {
        pname = "agent-run";
        inherit version;

        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.lock
            ./Cargo.toml
            ./build.rs
            ./src
          ];
        };

        cargoLock.lockFile = ./Cargo.lock;
        buildFeatures = ["external-bwrap"];
        BUBBLEWRAP_PATH = "${pkgs.bubblewrap}/bin/bwrap";

        # The integration tests require nested user namespaces and PTYs,
        # which are not reliably available on Nix builders.
        doCheck = false;

        strictDeps = true;

        meta = {
          description = "Run a coding agent in a sandboxed environment";
          homepage = "https://github.com/sin-ack/agent-run";
          license = pkgs.lib.licenses.gpl3Only;
          mainProgram = "agent-run";
          platforms = pkgs.lib.platforms.linux;
        };
      };
  in {
    packages = forAllSystems (
      system: let
        pkgs = import nixpkgs {inherit system;};
        agent-run = mkAgentRun pkgs;
      in {
        inherit agent-run;
        default = agent-run;
      }
    );

    apps = forAllSystems (system: {
      default = {
        type = "app";
        program = "${self.packages.${system}.default}/bin/agent-run";
        meta.description = "Run a coding agent in a sandboxed environment";
      };
    });

    checks = forAllSystems (system: {
      default = self.packages.${system}.default;
    });
  };
}
