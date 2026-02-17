{
  description = "Noxy — HTTP forward and reverse proxy with pluggable tower middleware";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, crane, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;

        src = pkgs.lib.cleanSourceWith {
          src = craneLib.path ./.;
          filter = path: type:
            (craneLib.filterCargoSources path type)
            || (pkgs.lib.hasSuffix ".md" path)
            || (pkgs.lib.hasSuffix ".js" path);
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = with pkgs; [ cmake ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
            pkgs.libiconv
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
          ];
          doCheck = false;
        };

        cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
          cargoExtraArgs = "--features cli";
        });

        noxy = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          cargoExtraArgs = "--features cli";
        });

        # Prebuilt V8 static library for the scripting feature (deno_core embeds V8).
        # Nix builds run in a sandbox without network access, so we prefetch the
        # archive and point the v8 crate's build script at it via RUSTY_V8_ARCHIVE.
        v8Version = "145.0.0";
        rustTarget = {
          x86_64-linux = "x86_64-unknown-linux-gnu";
          aarch64-linux = "aarch64-unknown-linux-gnu";
          x86_64-darwin = "x86_64-apple-darwin";
          aarch64-darwin = "aarch64-apple-darwin";
        }.${system} or (throw "noxy-scripting: unsupported system ${system}");

        v8Archive = pkgs.fetchurl {
          url = "https://github.com/denoland/rusty_v8/releases/download/v${v8Version}/librusty_v8_release_${rustTarget}.a.gz";
          hash = {
            x86_64-linux = "sha256-chV1PAx40UH3Ute5k3lLrgfhih39Rm3KqE+mTna6ysE=";
            aarch64-linux = "sha256-4IivYskhUSsMLZY97+g23UtUYh4p5jk7CzhMbMyqXyY=";
            x86_64-darwin = "sha256-1jUuC+z7saQfPYILNyRJanD4+zOOhXU2ac/LFoytwho=";
            aarch64-darwin = "sha256-yHa1eydVCrfYGgrZANbzgmmf25p7ui1VMas2A7BhG6k=";
          }.${system};
        };

        scriptingArgs = commonArgs // {
          RUSTY_V8_ARCHIVE = v8Archive;
        };

        cargoArtifactsScripting = craneLib.buildDepsOnly (scriptingArgs // {
          cargoExtraArgs = "--features cli,scripting";
        });

        noxyScripting = craneLib.buildPackage (scriptingArgs // {
          cargoArtifacts = cargoArtifactsScripting;
          cargoExtraArgs = "--features cli,scripting";
        });
      in
      {
        packages = {
          default = noxy;
          inherit noxy;
          noxy-scripting = noxyScripting;
        };

        devShells.default = craneLib.devShell {
          inputsFrom = [ noxy ];
        };
      }
    ) // {
      overlays.default = final: prev: {
        noxy = self.packages.${final.system}.default;
      };
    };
}
