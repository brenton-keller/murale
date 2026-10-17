{
  description = "Murale";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-compat = {
      url = "github:NixOS/flake-compat";
      flake = false;
    };
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSys = nixpkgs.lib.genAttrs supportedSystems;
      nixpkgsFor = forAllSys (system: import nixpkgs { inherit system; });
    in
    {
      packages = forAllSys (
        system:
        let
          pkgs = nixpkgsFor.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "murale";
            version = "1.0.0";

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = with pkgs; [
              pkg-config
            ];

            buildInputs = with pkgs; [
              wayland
              libxkbcommon
              mpv-unwrapped
            ];

            meta = with pkgs.lib; {
              description = "Lean, memory-safe video wallpaper player for Wayland compositors";
              homepage = "https://github.com/brenton-keller/murale";
              license = licenses.mit;
              mainprogram = "murale";
              platforms = platforms.linux;
            };
          };
        }
      );

      devShells = forAllSys (
        system:
        let
          pkgs = nixpkgsFor.${system};
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [
              self.packages.${system}.default
            ];

            buildInputs = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
            ];

            RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
          };
        }
      );
    };
}
