{
  description = "A YNAB updater";

  inputs.rustOverlay.url = "github:oxalica/rust-overlay";
  inputs.agenix.url = "github:ryantm/agenix";

  outputs =
    {
      self,
      nixpkgs,
      rustOverlay,
      agenix,
    }:
    let
      system = "x86_64-linux";

      pname = "ynab-updater";

      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rustOverlay.overlays.default ];
      };

      rust = pkgs.rust-bin.nightly.latest.default.override {
        extensions = [
          "rust-src"
          "clippy-preview"
          "rustfmt-preview"
        ];
      };

      rustPlatform = pkgs.makeRustPlatform {
        cargo = rust;
        rustc = rust;
      };

      ynab-updater = rustPlatform.buildRustPackage {
        inherit pname;

        version = "0.0.1";

        src = ./.;

        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [ pkgs.pkg-config ];

        buildInputs = [ pkgs.openssl ];
      };
    in
    with pkgs;
    {
      packages.${system} = {
        hl = writeShellScriptBin "hl" ''
          RUST_LOG=info \
          RUST_BACKTRACE=1 \
          YNAB_CONFIG_PATH=''${YNAB_CONFIG_PATH:-/home/james/dev/my/ynab_updater} \
          ${ynab-updater}/bin/hl
        '';
        saxo = writeShellScriptBin "saxo" ''
          RUST_LOG=info \
          RUST_BACKTRACE=1 \
          YNAB_TAILSCALE_IP=$(${pkgs.tailscale}/bin/tailscale ip --4) \
          YNAB_CONFIG_PATH=''${YNAB_CONFIG_PATH:-/home/james/dev/my/ynab_updater} \
          ${ynab-updater}/bin/saxo
        '';
      };

      devShell.${system} =
        let
          decrypt-local-config = writeShellScriptBin "decrypt-local-config" ''
            set -euo pipefail

            config_path=''${YNAB_CONFIG_PATH:-/home/james/dev/my/ynab_updater}

            (
              cd ${self}/secrets
              ${agenix.packages.${system}.default}/bin/agenix \
                -d settings.toml.age \
                -i ''${YNAB_AGE_IDENTITY:-$HOME/.ssh/id_ed25519}
            ) > "$config_path/settings.toml"

            echo "Decrypted settings.toml to $config_path/settings.toml"
          '';
        in
        mkShell {
          buildInputs = [
            rust-analyzer
            rust
            rustup
            pkg-config
            openssl
            agenix.packages.${system}.default
            decrypt-local-config
          ];
        };

      nixosModules.ynab-updater =
        { config, ... }:
        with lib;
        with lib.types;
        let
          cfg = config.programs.ynab-updater;
          secretName = "ynab-updater-settings";
          secretPath = "/run/ynab-updater-secrets/settings.toml";
          secretDir = dirOf secretPath;

          # Runs as root (the `+` on ExecStartPre) since issuing a Tailscale
          # cert needs either root or being the tailscaled "operator" - and
          # ynab-updater isn't made the operator, so as not to take that role
          # away from any human user who already has it. Safe to run on every
          # start: if the existing cert is still valid for a while,
          # `tailscale cert` just rewrites the same files.
          renewSaxoTlsCert = pkgs.writeShellScript "ynab-updater-saxo-renew-cert" ''
            set -euo pipefail

            dns_name=$(${pkgs.tailscale}/bin/tailscale status --json \
              | ${pkgs.jq}/bin/jq -r '.Self.DNSName | rtrimstr(".")')

            ${pkgs.tailscale}/bin/tailscale cert \
              --cert-file=${secretDir}/tailscale.crt \
              --key-file=${secretDir}/tailscale.key \
              "$dns_name"

            chown ynab-updater:ynab-updater ${secretDir}/tailscale.crt ${secretDir}/tailscale.key
            chmod 0400 ${secretDir}/tailscale.crt ${secretDir}/tailscale.key
          '';
        in
        {
          imports = [
            agenix.nixosModules.default
          ];

          options.programs.ynab-updater = {
            enable = mkEnableOption "Enable the YNAB updater service.";
            configDir = mkOption {
              type = types.str;
              description = lib.mdDoc "The directory of the config file & cache.";
            };
          };

          config = mkIf cfg.enable {
            users.users.ynab-updater = {
              isSystemUser = true;
              group = "ynab-updater";
            };
            users.groups.ynab-updater = { };

            age.secrets.${secretName} = {
              file = ./secrets/settings.toml.age;
              path = secretPath;
              mode = "0400";
              owner = "ynab-updater";
              group = "ynab-updater";
            };

            systemd.timers."ynab-updater-hl" = {
              wantedBy = [ "timers.target" ];
              timerConfig = {
                OnBootSec = "10s";
                OnUnitActiveSec = "24h";
                Unit = "ynab-updater-hl.service";
              };
            };
            systemd.services."ynab-updater-hl" = {
              environment = {
                RUST_LOG = "info";
                YNAB_CONFIG_PATH = secretDir;
              };
              serviceConfig = {
                Type = "oneshot";
                ExecStart = "${self.packages.${system}.hl}/bin/hl";
                User = "ynab-updater";
                Group = "ynab-updater";
                # Hardening: even a compromised process running as this
                # user gets a locked-down environment, and no other user
                # (besides root) can read the secret regardless.
                NoNewPrivileges = true;
                PrivateTmp = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                ProtectKernelTunables = true;
                ProtectKernelModules = true;
                ProtectControlGroups = true;
                RestrictSUIDSGID = true;
                RestrictNamespaces = true;
                LockPersonality = true;
                MemoryDenyWriteExecute = true;
              };
            };

            systemd.timers."ynab-updater-saxo" = {
              wantedBy = [ "timers.target" ];
              timerConfig = {
                OnBootSec = "10s";
                # 55m since the refresh_token duration is 1h
                # - so we want to refresh it before it expires
                OnUnitActiveSec = "55m";
                Unit = "ynab-updater-saxo.service";
              };
            };
            systemd.services."ynab-updater-saxo" = {
              environment = {
                RUST_LOG = "info";
                YNAB_CONFIG_PATH = secretDir;
              };
              serviceConfig = {
                Type = "oneshot";
                ExecStartPre = "+${renewSaxoTlsCert}";
                ExecStart = "${self.packages.${system}.saxo}/bin/saxo";
                User = "ynab-updater";
                Group = "ynab-updater";
                # The cert/key (and access_token.json) are written into the
                # otherwise-read-only secretDir.
                ReadWritePaths = [ secretDir ];
                NoNewPrivileges = true;
                PrivateTmp = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                ProtectKernelTunables = true;
                ProtectKernelModules = true;
                ProtectControlGroups = true;
                RestrictSUIDSGID = true;
                RestrictNamespaces = true;
                LockPersonality = true;
                MemoryDenyWriteExecute = true;
              };
            };
          };
        };
    };
}
