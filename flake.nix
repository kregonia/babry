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
