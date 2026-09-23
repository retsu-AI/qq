{ inputs, ... }:
{
  perSystem =
    { lib, pkgs, ... }:
    let
      toolchain = pkgs.qq-rust-toolchain;
      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };
      manifest = lib.importTOML ../Cargo.toml;
      version = manifest.workspace.package.version;
      self = inputs.self;
      # `build.rs` embeds the revision into `qq --version` from these three
      # variables when `.git` is absent, which it is inside the sandbox. A
      # dirty tree has no `rev`; the build then prints `unknown`.
      revisionEnv = lib.optionalAttrs (self ? rev) {
        QQ_GIT_SHA = builtins.substring 0 7 self.rev;
        QQ_GIT_FULL_SHA = self.rev;
        QQ_GIT_DATE =
          let
            d = self.lastModifiedDate;
          in
          "${builtins.substring 0 4 d}-${builtins.substring 4 2 d}-${builtins.substring 6 2 d}";
      };
      qq = rustPlatform.buildRustPackage (
        {
          pname = "qq";
          inherit version;
          src = lib.cleanSource ../.;
          cargoLock.lockFile = ../Cargo.lock;
          cargoBuildFlags = [
            "--bin"
            "qq"
          ];

          # aws-lc-sys drives its own CMake build from build.rs; the cmake
          # setup hook must not configure the workspace root.
          nativeBuildInputs = with pkgs; [
            cmake
            pkg-config
          ];
          dontUseCmakeConfigure = true;

          # The workspace suite (~1k tests) needs the release profile's
          # fixtures and takes minutes; CI runs it on every PR, so the package
          # build only asserts the binary starts.
          doCheck = false;
          doInstallCheck = true;
          installCheckPhase = ''
            runHook preInstallCheck
            "$out/bin/qq" --version | grep -q "^qq ${version} "
            runHook postInstallCheck
          '';

          meta = {
            description = "AI coding agents in one binary: terminal UI, headless runner, and local server";
            homepage = "https://github.com/retsu-AI/qq";
            license = lib.licenses.mit;
            mainProgram = "qq";
            platforms = lib.platforms.linux ++ lib.platforms.darwin;
          };
        }
        // revisionEnv
      );
    in
    {
      packages = {
        inherit qq;
        default = qq;
        rust-toolchain = toolchain;
      };
      apps.default = {
        type = "app";
        program = "${qq}/bin/qq";
        meta.description = "Run qq";
      };
    };
}
