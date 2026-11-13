#pragma once

namespace Autostart
{
  bool isEnabled();
  bool setEnabled(bool enabled);
  // An entry added from Plasma's Autostart page is a copy of the menu entry: it opens the
  // window instead of starting hidden. An enabled entry that does that is rewritten to the
  // one `setEnabled(true)` writes. A disabled entry (Hidden=true) is left alone.
  void repair();
}
