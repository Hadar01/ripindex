# Homebrew formula for a tap (e.g. github.com/hadar01/homebrew-tap).
#
# Copy this to Formula/ripindex.rb in your tap repo, then fill in the four
# sha256 values from the release's SHA256SUMS:
#
#   curl -sL https://github.com/hadar01/ripindex/releases/download/v0.1.0/SHA256SUMS
#
# Installed via:  brew install hadar01/tap/ripindex
class Ripindex < Formula
  desc "Indexed code and text search with a crash-safe on-disk index and a background daemon"
  homepage "https://github.com/hadar01/ripindex"
  version "0.1.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/hadar01/ripindex/releases/download/v#{version}/ripindex-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_aarch64_apple_darwin"
    end
    on_intel do
      url "https://github.com/hadar01/ripindex/releases/download/v#{version}/ripindex-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_x86_64_apple_darwin"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hadar01/ripindex/releases/download/v#{version}/ripindex-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_SHA256_aarch64_unknown_linux_gnu"
    end
    on_intel do
      url "https://github.com/hadar01/ripindex/releases/download/v#{version}/ripindex-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_SHA256_x86_64_unknown_linux_gnu"
    end
  end

  def install
    bin.install "ripindex"
    doc.install "README.md", "CHANGELOG.md", "FORMAT.md"
  end

  def caveats
    <<~EOS
      ripindex starts its daemon on demand; nothing is installed to run at login.
      To start it at login anyway, see:  ripindex daemon install-hint
    EOS
  end

  test do
    # Index a tiny corpus and assert the term is found - exercises the real
    # write/commit/read path, not just --version.
    (testpath/"a.txt").write "alpha beta gamma\n"
    system bin/"ripindex", "index", testpath
    assert_match "a.txt", shell_output("#{bin}/ripindex search --no-daemon --root #{testpath} beta")
  end
end
