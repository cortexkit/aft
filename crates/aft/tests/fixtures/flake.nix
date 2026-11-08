{
  description = "demo flake";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs";

  outputs = inputs@{ self, nixpkgs }: {
    devShells.x86_64-linux.default = nixpkgs.legacyPackages.x86_64-linux.mkShell { };
  };
}
