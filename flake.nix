{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils, naersk }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
      in
      {
        defaultPackage = naersk-lib.buildPackage {
          src = ./.;

          nativeBuildInputs = with pkgs; [
            pkg-config
            clang
            llvmPackages.libclang
          ];

          buildInputs = with pkgs; [
            sqlite
            zlib
            openssl
          ];

          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";
        };

        devShell = with pkgs; mkShell {
          buildInputs = [
            cargo
            rustc
            rustfmt
            pre-commit
            rustPackages.clippy

            pkg-config

            sqlite
            zlib
            openssl

            clang
            llvmPackages.libclang
          ];

          RUST_SRC_PATH = rustPlatform.rustLibSrc;

          LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
          LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";
        };
      }
    );
}
