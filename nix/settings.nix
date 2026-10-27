# Shared by the NixOS and Home Manager modules: the `settings` option (one
# attribute per key of config.toml) and the config file built from it.
{ lib, pkgs }:

let
  inherit (lib) mkOption types;
  format = pkgs.formats.toml { };
  positive = types.ints.positive;
in
{
  settingsOption =
    {
      dbPathDefault,
      dbPathDefaultText ? dbPathDefault,
    }:
    mkOption {
      description = ''
        isp-snitch configuration, written to config.toml. See
        `deploy/config.example.toml` in the isp-snitch repository. Keys the
        module does not know are passed through and checked at build time.
      '';
      default = { };
      type = types.submodule {
        freeformType = format.type;
        options = {
          interface = mkOption {
            type = types.nullOr types.nonEmptyStr;
            default = null;
            example = "enp5s0";
            description = ''
              Device every probe leaves through (`SO_BINDTODEVICE`). `null`
              follows the routing table, e.g. on a single-NIC host.
            '';
          };
          listen = mkOption {
            type = types.strMatching ".+:[0-9]+";
            default = "127.0.0.1:9180";
            example = "0.0.0.0:9180";
            description = ''
              Address and port of the HTTP UI and API. There is no login, so
              listen beyond loopback only on trusted networks.
            '';
          };
          db_path = mkOption {
            type = types.str;
            default = dbPathDefault;
            defaultText = dbPathDefaultText;
            description = "SQLite database file. Its directory is created if missing.";
          };
          probe = mkOption {
            default = { };
            description = "Probe timing.";
            type = types.submodule {
              freeformType = format.type;
              options = {
                interval_ms = mkOption {
                  type = positive;
                  default = 1000;
                  description = "One round (a probe per hop) per target, this often.";
                };
                timeout_ms = mkOption {
                  type = positive;
                  default = 2000;
                  description = "A probe with no answer after this counts as lost.";
                };
                max_ttl = mkOption {
                  type = types.ints.between 1 64;
                  default = 30;
                  description = "Highest TTL probed while the target distance is unknown.";
                };
                probe_spacing_ms = mkOption {
                  type = types.ints.unsigned;
                  default = 5;
                  description = "Gap between the probes of one round.";
                };
              };
            };
          };
          retention = mkOption {
            default = { };
            description = "How long data are kept.";
            type = types.submodule {
              freeformType = format.type;
              options = {
                minute_days = mkOption {
                  type = positive;
                  default = 90;
                  description = "Days of full 1-minute detail.";
                };
                hour_days = mkOption {
                  type = positive;
                  default = 365;
                  description = "Days of hourly rollups.";
                };
              };
            };
          };
          targets = mkOption {
            description = "Hosts to trace. IPv4 only; addresses must be unique.";
            default = [
              {
                name = "Cloudflare";
                addr = "1.1.1.1";
              }
              {
                name = "Google";
                addr = "8.8.8.8";
              }
              {
                name = "Quad9";
                addr = "9.9.9.9";
              }
            ];
            type = types.nonEmptyListOf (
              types.submodule {
                options = {
                  name = mkOption {
                    type = types.str;
                    description = "Label shown in the UI.";
                  };
                  addr = mkOption {
                    type = types.str;
                    description = "IPv4 address.";
                  };
                };
              }
            );
          };
        };
      };
    };

  # Validated with the binary's own `check-config`, so a bad setting fails the
  # build, not the service start. Skipped when the build host cannot run it.
  configFile =
    package: settings:
    let
      raw = format.generate "isp-snitch.toml" (lib.filterAttrsRecursive (_: v: v != null) settings);
    in
    if pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform then
      pkgs.runCommand "isp-snitch-config.toml" { } ''
        ${lib.getExe package} check-config --config ${raw} > /dev/null
        cp ${raw} $out
      ''
    else
      raw;

  # Port of a `listen` value such as "0.0.0.0:9180" or "[::]:9180".
  listenPort = listen: lib.toInt (lib.last (lib.splitString ":" listen));
}
