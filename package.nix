{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "noxy";
  version = "0.0.5";

  src = ./.;

  cargoHash = lib.fakeHash;

  buildFeatures = [ "cli" ];

  doCheck = false;

  meta = with lib; {
    description = "HTTP forward and reverse proxy with pluggable tower middleware";
    homepage = "https://github.com/reu/noxy";
    license = licenses.mit;
    mainProgram = "noxy";
  };
}
