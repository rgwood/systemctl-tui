{ lib
, rustPlatform
, stdenv
, darwin
, nix-update-script
, testers
, systemctl-tui
,
}:
let
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);
in
rustPlatform.buildRustPackage rec {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;

  src = builtins.path {
    path = ../.;
  };

  cargoLock.lockFile = ../Cargo.lock;

  buildInputs = lib.optionals stdenv.hostPlatform.isDarwin [ darwin.apple_sdk.frameworks.AppKit ];

  passthru = {
    updateScript = nix-update-script;
    tests.version = testers.testVersion { package = systemctl-tui; };
  };

  meta = {
    description = cargoToml.package.description;
    homepage = cargoToml.package.homepage;
    changelog = "https://github.com/rgwood/systemctl-tui/releases/tag/v${version}";
    license = lib.licenses.mit;
    mainProgram = cargoToml.package.name;
  };
}
