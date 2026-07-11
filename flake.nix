{
  description = "Save, load, and manage Total War: WARHAMMER III mod profiles (Proton/Steam)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        twwh3-profile = pkgs.writeShellApplication {
          name = "twwh3-profile";
          runtimeInputs = [ pkgs.coreutils ];
          text = builtins.readFile ./twwh3-profile.sh;
          meta = {
            description = "Save, load, and manage Total War: WARHAMMER III mod profiles";
            homepage = "https://github.com/xalayn/TWW3-Mod-Profile-Manager";
            license = nixpkgs.lib.licenses.mit;
            mainProgram = "twwh3-profile";
            platforms = systems;
          };
        };
        twwh3-run = pkgs.writeShellApplication {
          name = "twwh3-run";
          # fuse-overlayfs mounts the mod overlay; util-linux provides
          # mountpoint. fusermount3 is intentionally NOT bundled: the
          # store copy isn't setuid, and bundling it would shadow the
          # working /run/wrappers/bin wrapper on NixOS.
          runtimeInputs = [ pkgs.coreutils pkgs.util-linux pkgs.fuse-overlayfs ];
          text = builtins.readFile ./twwh3-run.sh;
          meta = {
            description = "Steam launch-option shim for Total War: WARHAMMER III (use: twwh3-run %command%)";
            homepage = "https://github.com/xalayn/TWW3-Mod-Profile-Manager";
            license = nixpkgs.lib.licenses.mit;
            mainProgram = "twwh3-run";
            platforms = systems;
          };
        };

        twwh3-mods = pkgs.rustPlatform.buildRustPackage {
          pname = "twwh3-mods";
          version = "0.3.1";
          src = ./tui;
          cargoLock.lockFile = ./tui/Cargo.lock;
          meta = {
            description = "TUI mod load-order manager for Total War: WARHAMMER III";
            homepage = "https://github.com/xalayn/TWW3-Mod-Profile-Manager";
            license = nixpkgs.lib.licenses.mit;
            mainProgram = "twwh3-mods";
            platforms = systems;
          };
        };

        # Everything in one output: result/bin/{twwh3-profile,twwh3-mods,twwh3-run}
        default = pkgs.symlinkJoin {
          name = "twwh3-tools";
          paths = [ twwh3-profile twwh3-mods twwh3-run ];
          meta = {
            description = "Mod profile manager, TUI, and launch shim for Total War: WARHAMMER III";
            homepage = "https://github.com/xalayn/TWW3-Mod-Profile-Manager";
            license = nixpkgs.lib.licenses.mit;
            platforms = systems;
          };
        };
      });

      overlays.default = final: prev: {
        twwh3-profile = self.packages.${final.system}.twwh3-profile;
        twwh3-mods = self.packages.${final.system}.twwh3-mods;
        twwh3-run = self.packages.${final.system}.twwh3-run;
      };
    };
}
