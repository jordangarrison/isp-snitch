# VM test of the NixOS module.
#
# The client traces the target through the router, with probes bound to eth1
# (eth0 is the test driver's network and has no route to the target), so the
# trace shows two hops.
self:
{ lib, ... }:

let
  addrOf = node: iface: (lib.head node.networking.interfaces.${iface}.ipv4.addresses).address;
in
{
  name = "isp-snitch";

  nodes = {
    router =
      { pkgs, ... }:
      {
        virtualisation.vlans = [
          1
          2
        ];
        boot.kernel.sysctl."net.ipv4.ip_forward" = 1;
        environment.systemPackages = [ pkgs.curl ];
      };

    target =
      { nodes, ... }:
      {
        virtualisation.vlans = [ 2 ];
        networking.interfaces.eth1.ipv4.routes = [
          {
            address = "192.168.1.0";
            prefixLength = 24;
            via = addrOf nodes.router "eth2";
          }
        ];
      };

    client =
      { nodes, pkgs, ... }:
      {
        imports = [ self.nixosModules.default ];
        virtualisation.vlans = [ 1 ];
        networking.interfaces.eth1.ipv4.routes = [
          {
            address = "192.168.2.0";
            prefixLength = 24;
            via = addrOf nodes.router "eth1";
          }
        ];
        environment.systemPackages = [
          pkgs.curl
          pkgs.sqlite
        ];

        services.isp-snitch = {
          enable = true;
          openFirewall = true;
          settings = {
            interface = "eth1";
            listen = "0.0.0.0:9181";
            probe.max_ttl = 8;
            retention.minute_days = 7;
            targets = [
              {
                name = "target";
                addr = addrOf nodes.target "eth1";
              }
            ];
          };
        };
      };
  };

  testScript =
    { nodes, ... }:
    let
      routerAddr = addrOf nodes.router "eth1";
      targetAddr = addrOf nodes.target "eth1";
      clientAddr = addrOf nodes.client "eth1";
    in
    ''
      import json

      start_all()
      client.wait_for_unit("isp-snitch.service")
      client.wait_for_open_port(9181)

      with subtest("runs as a dynamic user with no capabilities"):
          show = client.succeed("systemctl show isp-snitch -p DynamicUser -p CapabilityBoundingSet -p AmbientCapabilities")
          assert "DynamicUser=yes\n" in show, show
          assert "CapabilityBoundingSet=\n" in show, show
          assert "AmbientCapabilities=\n" in show, show
          pid = client.succeed("systemctl show isp-snitch -p MainPID --value").strip()
          caps = client.succeed(f"grep -E '^Cap(Inh|Prm|Eff|Bnd|Amb)' /proc/{pid}/status")
          assert all(line.split()[1] == "0000000000000000" for line in caps.splitlines()), caps

      with subtest("config comes from the module settings"):
          status = json.loads(client.succeed("curl -sf localhost:9181/api/status"))
          assert status["interface"] == "eth1", status

      with subtest("rows are flushed to SQLite"):
          client.wait_until_succeeds("journalctl -u isp-snitch -o cat | grep -q probe_flush", timeout=150)
          client.succeed("sqlite3 /var/lib/isp-snitch/isp-snitch.db 'select count(*) from hop_minute' | grep -qv '^0$'")

      with subtest("the bound trace goes through the router to the target"):
          hops = json.loads(client.succeed("curl -sf localhost:9181/api/targets/1/hops"))["hops"]
          addrs = {h["ttl"]: h["addr"] for h in hops}
          assert addrs.get(1) == "${routerAddr}", hops
          assert addrs.get(2) == "${targetAddr}", hops

      with subtest("openFirewall opens the listen port"):
          router.succeed("curl -sf http://${clientAddr}:9181/healthz")
    '';
}
