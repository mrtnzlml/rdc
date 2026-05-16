class Rdc < Formula
  desc "Rossum Deployment as Code -- CLI for snapshotting and deploying Rossum.ai configurations"
  homepage "https://github.com/mrtnzlml/rossum-deployment-manager-experiment"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/mrtnzlml/rossum-deployment-manager-experiment/releases/download/v0.1.0/rdc-aarch64-apple-darwin.tar.gz"
      sha256 "ff0d71c63446cdd60d9fe463e4c6ef16f5aad27e8adf2e77a83c92f7c62268e3"
    end
    on_intel do
      url "https://github.com/mrtnzlml/rossum-deployment-manager-experiment/releases/download/v0.1.0/rdc-x86_64-apple-darwin.tar.gz"
      sha256 "26a97c1dae6e1b47a5dac7ca95507fe3aa5c6d3e9b354b1926626182724e7844"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/mrtnzlml/rossum-deployment-manager-experiment/releases/download/v0.1.0/rdc-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "92f557d566bcda311a2c4c2b2834d9c7635788892e0d3fbde8a800ec1f1d6332"
    end
  end

  def install
    bin.install "rdc"
  end

  test do
    assert_match "rdc", shell_output("#{bin}/rdc --version")
  end
end
