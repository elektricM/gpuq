# Nix

`package.nix` follows the nixpkgs layout for `pkgs/by-name/gp/gpuq/package.nix`.

Build it from this directory:

```sh
nix-build -E 'with import <nixpkgs> { }; callPackage ./package.nix { }'
```

## nixpkgs pull request

1. Add a maintainer entry to `maintainers/maintainer-list.nix`, in alphabetical order, in a
   separate commit (`maintainers: add elektricM`):

   ```nix
   elektricM = {
     name = "Martin Dolez";
     email = "martin@dolez.fr";
     github = "elektricM";
     githubId = <numeric id from https://api.github.com/users/elektricM>;
   };
   ```

2. Copy `package.nix` to `pkgs/by-name/gp/gpuq/package.nix`, set
   `maintainers = with lib.maintainers; [ elektricM ];`, drop the two SPDX and copyright
   comment lines (nixpkgs files are covered by its MIT license), and commit it as
   `gpuq: init at 0.3.0`.

3. Check it builds and passes the nixpkgs checks:

   ```sh
   nix-build -A gpuq
   nix-shell -p nixpkgs-review --run "nixpkgs-review wip"
   nix fmt   # nixfmt-rfc-style
   ```

4. Open the pull request against `master` and fill in the template (tested platforms,
   `nixpkgs-review` result).

A NixOS module (`services.gpuq`) can be proposed later in a separate pull request.
