{
  description = "Babry - Wayland screenshot tool";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      packageFor = system:
        let
          pkgs = import nixpkgs { inherit system; };
          inherit (pkgs) lib;
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "babry";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [
            makeWrapper
            pkg-config
          ];

          buildInputs = with pkgs; [
            fontconfig
            libxkbcommon
            systemd
            wayland
          ];

          postInstall = ''
            wrapProgram $out/bin/babry \
              --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ pkgs.systemd ]} \
              --prefix PATH : ${lib.makeBinPath [ pkgs.fontconfig ]}
          '';

          meta = {
            description = "Wayland screenshot tool with smart region selection and annotation";
            homepage = "https://github.com/kregonia/babry";
            license = lib.licenses.asl20;
            mainProgram = "babry";
            platforms = systems;
          };
        };
    in
    {
      packages = forAllSystems (system: {
        default = packageFor system;
        babry = packageFor system;
      });

      nixosModules = {
        default = { config, lib, pkgs, ... }:
          let
            cfg = config.programs.babry;
          in
          {
            options.programs.babry = {
              enable = lib.mkEnableOption "Babry Wayland screenshot tool";

              package = lib.mkOption {
                type = lib.types.package;
                default = packageFor pkgs.system;
                defaultText = lib.literalExpression "babry.packages.\${pkgs.system}.default";
                description = "Babry package to install and run.";
              };
            };

            config = lib.mkIf cfg.enable {
              environment.systemPackages = [ cfg.package ];

              systemd.user.services.babry = {
                description = "Babry Wayland screenshot daemon";
                wantedBy = [ "graphical-session.target" ];
                partOf = [ "graphical-session.target" ];
                after = [ "graphical-session.target" ];
                unitConfig = {
                  StartLimitIntervalSec = 60;
                  StartLimitBurst = 5;
                };
                serviceConfig = {
                  ExecStart = "${cfg.package}/bin/babry --daemon";
                  Restart = "on-failure";
                  RestartSec = "5s";
                };
              };
            };
          };

        babry = self.nixosModules.default;
      };

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${packageFor system}/bin/babry";
          meta.description = "Run Babry";
        };
      });

      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              fontconfig
              libxkbcommon
              pkg-config
              rustc
              rustfmt
              systemd
              wayland
            ];
          };
        });
    };
}
