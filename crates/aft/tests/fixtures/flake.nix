{
  description = "demo flake";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs";

  outputs = inputs@{ self, nixpkgs }: {
    devShells.default = nixpkgs.legacyPackages.x86_64-linux.mkShell { };
  };
}
