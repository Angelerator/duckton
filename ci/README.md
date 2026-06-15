# CI workflow (staged here, not yet active)

`ci/ci.yml` is the project's GitHub Actions workflow. It is kept here (instead of
`.github/workflows/`) only because the token used to push lacked the `workflow`
OAuth scope, so it could not be committed under `.github/workflows/` directly.

To activate CI, copy it into place using **either**:

- **GitHub web UI** (no special token needed): in the repo, *Add file → Create new
  file*, name it `.github/workflows/ci.yml`, and paste the contents of `ci/ci.yml`.

- **A token with the `workflow` scope** (locally):
  ```bash
  gh auth refresh -h github.com -s workflow
  mkdir -p .github/workflows && git mv ci/ci.yml .github/workflows/ci.yml
  git commit -m "Enable CI workflow" && git push
  ```

Once active you can delete this `ci/` directory.
