{ inputs, ... }: {
  perSystem =
    {
      lib,
      pkgs,
      system,
      ...
    }:
    let
      commonTools = with pkgs; [
        fd
        git
        hyperfine
        jq
        just
        ripgrep
      ];

      nixTools = with pkgs; [
        deadnix
        nixd
        nixfmt
        statix
      ];

      rustTools = with pkgs; [
        qq-rust-toolchain
        bacon
        cargo-audit
        cargo-deny
        cargo-edit
        cargo-expand
        cargo-llvm-cov
        cargo-machete
        cargo-nextest
        cargo-outdated
        ron-lsp
        sccache
        taplo
      ];

      webTools = [
        pkgs.biome
        inputs.nub.packages.${system}.nub
        pkgs.nodejs_24
        pkgs.typescript
        pkgs.typescript-language-server
        pkgs.vscode-langservers-extracted
      ];

      nativeTools =
        (with pkgs; [
          cmake
          ninja
          pkg-config
        ])
        ++ lib.optionals pkgs.stdenv.isLinux (
          with pkgs.llvmPackages;
          [
            clang
            lld
            lldb
          ]
        );

      nativeLibraries =
        (with pkgs; [
          openssl
          sqlite
          zlib
        ])
        ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.llvmPackages.libclang ]
        ++ lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];

      # Terminal-Bench evaluation: Harbor runs under uv, and the qq binary
      # uploaded into task containers is a static x86_64 musl build so it
      # runs on any task image regardless of its glibc.
      evalTools = with pkgs; [
        python313
        uv
      ];

      muslCross = pkgs.pkgsCross.musl64.stdenv.cc;

      evalShellHook = lib.optionalString (pkgs.stdenv.isLinux && pkgs.stdenv.isx86_64) ''
        export CC_x86_64_unknown_linux_musl=${muslCross}/bin/${muslCross.targetPrefix}cc
        export AR_x86_64_unknown_linux_musl=${muslCross}/bin/${muslCross.targetPrefix}ar
        export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=${muslCross}/bin/${muslCross.targetPrefix}cc
      '';

      mkRustShell =
        {
          name,
          extraPackages ? [ ],
        }:
        pkgs.mkShell {
          inherit name;
          packages = commonTools ++ nixTools ++ rustTools ++ nativeTools ++ evalTools ++ extraPackages;
          buildInputs = nativeLibraries;
          shellHook = evalShellHook;
        };
    in
    {
      devShells = {
        default = mkRustShell {
          name = "qq";
          extraPackages = webTools;
        };

        rust = mkRustShell {
          name = "qq-rust";
        };

        web = pkgs.mkShellNoCC {
          name = "qq-web";
          packages = commonTools ++ nixTools ++ webTools;
        };
      };

      formatter = pkgs.nixfmt;
    };
}
