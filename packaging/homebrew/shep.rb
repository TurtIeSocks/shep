# The formula published to the tap at shep-pm/homebrew-shep, where it
# lives as Formula/shep.rb. This copy is the source of truth; the `homebrew`
# job in .github/workflows/release-artifacts.yml rewrites the two version
# lines and pushes the result to the tap on every release.
#
# Builds from source rather than from the release archives, which is
# deliberate for now. Bumping a source formula is one url and one sha256, both
# of which crates.io publishes; a binary formula carries a url and sha256 per
# platform and has nothing to point at until release-artifacts.yml has run
# against a real release. docs/distribution.md records when to switch.
class Shep < Formula
  desc "Process manager that keeps a flock of long-running processes alive"
  homepage "https://shep-pm.com"
  # The crate tarball rather than a GitHub tag archive. Its sha256 is
  # published by the crates.io API as the version's `checksum`, so the bump
  # job reads the hash instead of downloading and computing one. GitHub
  # generates archive tarballs on the fly, which is the weaker guarantee of
  # the two.
  url "https://static.crates.io/crates/shep/shep-0.1.12.crate"
  sha256 "c358d54f1700af49d528cd2a1356835dfdd3959c911401e077508312d195b2a0"
  license any_of: ["Apache-2.0", "MIT"]

  livecheck do
    url :stable
    strategy :crate
  end

  depends_on "rust" => :build

  def install
    # Installs all three [[bin]] targets, the same set `cargo install shep`
    # puts on a machine. `std_cargo_args` supplies `--locked`, and the crate
    # tarball carries the Cargo.lock that needs.
    system "cargo", "install", *std_cargo_args

    # shep's form is `shep completions <shell>`, which is this helper's
    # default shape, so it needs no `shell_parameter_format`. The status line
    # shep prints for this verb goes to stderr, so the generated scripts stay
    # clean.
    generate_completions_from_executable(bin/"shep", "completions")
  end

  def caveats
    <<~EOS
      shep installs its own launchd job:

        shep startup      supervise the flock from login
        shep unstartup    undo that

      So this formula ships no `brew services` definition. Running both would
      leave two launchd jobs pointed at one shepherd.
    EOS
  end

  test do
    assert_match "shep #{version}", shell_output("#{bin}/shep --version")

    # Mirrors the crate's own `completions_cover_the_visible_aliases`: the
    # verbs come from shep's command tree rather than from clap_complete's
    # template, so this fails on a stub that a "script is non-empty" check
    # would pass.
    script = shell_output("#{bin}/shep completions bash")
    assert_match "flock", script
    assert_match "bleats", script

    # 5 is DaemonUnreachable. `ping` is the one verb that treats "nothing
    # answered" as information rather than as a failure, so this runs the
    # real socket path without starting a shepherd or leaving one behind.
    ENV["SHEP_HOME"] = testpath
    shell_output("#{bin}/shep ping", 5)
  end
end
