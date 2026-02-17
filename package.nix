{ lib, rustPlatform, cmake, pkg-config, openssl }:

rustPlatform.buildRustPackage {
  pname = "noxy";
  version = "0.0.4";

  src = ./.;

  cargoHash = "sha256-4WK2BvEsSRtH28CZUSyb8Vp84LqpiBzJGI8XNgdqjOI=";

  buildFeatures = [ "cli" ];

  nativeBuildInputs = [ cmake pkg-config ];
  buildInputs = [ openssl ];

  doCheck = false;

  meta = with lib; {
    description = "HTTP forward and reverse proxy with pluggable tower middleware";
    homepage = "https://github.com/reu/noxy";
    license = licenses.mit;
    mainProgram = "noxy";
  };
}
