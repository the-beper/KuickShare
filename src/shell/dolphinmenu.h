#pragma once

namespace DolphinMenu
{
  bool exists();
  bool setEnabled(bool enabled);
  // Makes the user's entry match the setting: writes it when on and neither the home nor a system folder has it, removes it when off and present.
  bool sync(bool enabled);
}
