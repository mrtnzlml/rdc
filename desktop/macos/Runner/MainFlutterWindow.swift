import Cocoa
import FlutterMacOS

class MainFlutterWindow: NSWindow {
  override func awakeFromNib() {
    let flutterViewController = FlutterViewController()
    self.contentViewController = flutterViewController

    // First-launch window size — wider than the 800×600 nib default to suit
    // the sidebar + main layout. macOS restores the user's size on later runs.
    self.setContentSize(NSSize(width: 1180, height: 780))
    self.center()

    RegisterGeneratedPlugins(registry: flutterViewController)

    super.awakeFromNib()
  }
}
