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
        twwh3-mods = pkgs.rustPlatform.buildRustPackage {
          pname = "twwh3-mods";
          version = "0.1.0";
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

        default = twwh3-profile;
      });

      overlays.default = final: prev: {
        twwh3-profile = self.packages.${final.system}.twwh3-profile;
        twwh3-mods = self.packages.${final.system}.twwh3-mods;
      };
    };
}
