{
  description = "isp-snitch: per-hop ISP packet-loss monitor";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        isp-snitch = pkgs.rustPlatform.buildRustPackage {
          pname = "isp-snitch";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # Live tests open ICMP sockets, which the Nix build sandbox does not allow.
          doCheck = false;
          meta.mainProgram = "isp-snitch";
        };
        default = isp-snitch;

        # Run with --network host (to see the probe interface) and mount a config
        # at /config/config.toml whose db_path points into /data.
        docker = pkgs.dockerTools.buildLayeredImage {
          name = "isp-snitch";
          tag = isp-snitch.version;
          contents = [ isp-snitch pkgs.cacert ];
          extraCommands = "mkdir -p data config";
          config = {
            Entrypoint = [ "${isp-snitch}/bin/isp-snitch" ];
            Cmd = [ "run" "--config" "/config/config.toml" ];
            Volumes = { "/data" = { }; };
            ExposedPorts = { "9180/tcp" = { }; };
          };
        };
      });
    };
}
