# Git & GitHub Reference

A concise command reference for everyday Git and GitHub usage.

---

## Setup

```bash
git config --global user.name "Your Name"
git config --global user.email "you@example.com"
git config --global core.editor "code --wait"   # VS Code as default editor
```

---

## Forking & Cloning

**Fork** — creates your own copy of someone else's repo on GitHub (done in the browser, or via CLI):
```bash
gh repo fork owner/repo --clone          # fork + clone in one step
```

**Clone** — download a repo to your machine:
```bash
git clone https://github.com/you/repo.git
git clone https://github.com/you/repo.git my-folder   # into a specific folder
```

---

## Daily Workflow

```bash
git status                    # show changed / staged files
git diff                      # show unstaged changes
git diff --staged             # show staged changes (ready to commit)

git add file.rs               # stage a specific file
git add .                     # stage all changes in current directory
git add -p                    # stage changes interactively (chunk by chunk)

git commit -m "Add feature X"
git commit --amend            # edit the last commit (message or content)
                              # WARNING: never amend commits already pushed

git push                      # push current branch to remote
git push origin main          # explicit: push branch 'main' to 'origin'
git push -u origin my-branch  # push new branch and set upstream (-u = --set-upstream)

git pull                      # fetch + merge remote changes into current branch
git pull --rebase             # fetch + rebase instead of merge (cleaner history)
```

---

## Branches

```bash
git branch                    # list local branches (* = current)
git branch -a                 # list all branches (including remote)

git branch feature/login      # create a new branch
git switch feature/login      # switch to it
git switch -c feature/login   # create + switch in one step

git merge feature/login       # merge branch into current branch
git merge --no-ff feature/login   # merge with a merge commit (preserves history)

git branch -d feature/login   # delete branch (only if merged)
git branch -D feature/login   # force delete

git push origin --delete feature/login   # delete branch on remote
```

---

## Remotes

A **remote** is a named reference to a repository URL.

```bash
git remote -v                          # list remotes with URLs
git remote add origin https://...      # add a remote named 'origin'
git remote add upstream https://...    # add original repo as 'upstream' (fork workflow)
git remote set-url origin https://...  # change the URL of a remote

git fetch origin                       # download remote changes without merging
git fetch --all                        # fetch all remotes
```

### Keeping a Fork in Sync

```bash
git fetch upstream                     # get latest from original repo
git switch main
git merge upstream/main                # bring your main up to date
git push origin main                   # push to your fork
```

---

## Tags

Tags mark a specific commit — typically used for releases.

```bash
git tag                          # list all tags
git tag v1.0.0                   # create a lightweight tag on HEAD
git tag -a v1.0.0 -m "Release 1.0.0"  # annotated tag (recommended — stores metadata)
git tag -a v1.0.0 abc1234        # tag a specific commit by hash

git push origin v1.0.0           # push a single tag
git push origin --tags           # push all tags

git tag -d v1.0.0                # delete tag locally
git push origin --delete v1.0.0  # delete tag on remote
```

---

## Pull Requests (GitHub CLI)

```bash
gh pr create                          # create PR interactively
gh pr create --title "Fix bug" --body "Description"
gh pr create --base main --head feature/login

gh pr list                            # list open PRs
gh pr view 42                         # view PR #42
gh pr checkout 42                     # check out PR #42 locally
gh pr merge 42 --merge                # merge PR
gh pr merge 42 --squash               # squash and merge
gh pr close 42                        # close without merging
```

---

## Releases (GitHub CLI)

```bash
gh release create v1.0.0                          # create release from tag
gh release create v1.0.0 --title "v1.0.0" --notes "Changelog..."
gh release create v1.0.0 dist/app.exe            # attach a file
gh release list                                    # list releases
```

---

## Undoing Things

```bash
git restore file.rs               # discard unstaged changes in a file
git restore --staged file.rs      # unstage a file (keep changes in working dir)

git revert abc1234                # create a new commit that undoes a commit (safe)
git reset --soft HEAD~1           # undo last commit, keep changes staged
git reset --hard HEAD~1           # undo last commit, DISCARD changes (destructive)

git stash                         # temporarily shelve uncommitted changes
git stash pop                     # restore stashed changes
git stash list                    # list all stashes
```

---

## Useful Extras

```bash
git log --oneline --graph         # compact visual history
git log --oneline -10             # last 10 commits
git show abc1234                  # show a specific commit

git blame file.rs                 # show who last changed each line

git cherry-pick abc1234           # apply a single commit onto current branch

git clean -fd                     # delete untracked files and directories (destructive)
```

### .gitignore

Create a `.gitignore` file in the repo root to exclude files from tracking:

```
target/
*.log
.env
```

```bash
git rm --cached file.rs           # stop tracking a file already committed
                                  # (add it to .gitignore first)
```

---

## Common Patterns

### Feature Branch Workflow
```bash
git switch -c feature/my-feature  # branch off main
# ... make changes ...
git add . && git commit -m "Implement feature"
git push -u origin feature/my-feature
gh pr create --base main          # open PR
```

### Fix a Mistake Before Pushing
```bash
git add forgotten-file.rs
git commit --amend --no-edit      # add file to last commit without changing message
```

### Sync Local with Remote (no conflicts)
```bash
git fetch origin
git rebase origin/main            # replay your commits on top of latest main
```
