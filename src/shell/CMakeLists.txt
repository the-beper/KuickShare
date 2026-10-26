cmake_minimum_required(VERSION 3.24)
project(kuickshare VERSION 1.3.3 LANGUAGES CXX)
add_compile_definitions(KUICKSHARE_VERSION="${PROJECT_VERSION}")

find_package(ECM REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(Qt6 6.9 REQUIRED COMPONENTS Core Gui Qml Quick QuickControls2 DBus Widgets Test)
find_package(KF6 REQUIRED COMPONENTS Kirigami I18n CoreAddons Crash DBusAddons StatusNotifierItem Notifications JobWidgets)
set(QT_MAJOR_VERSION 6)
include(ECMFindQmlModule)
ecm_find_qmlmodule(org.kde.kirigami REQUIRED)

qt_add_executable(kuickshare
    main.cpp
    corebridge.cpp
    transfermodel.cpp
    transferfiltermodel.cpp
    devicemodel.cpp
    autostart.cpp
    dolphinmenu.cpp
    integration.cpp
    transferjob.cpp
    transferwatcher.cpp
    dndwatcher.cpp
    instance.cpp
    updatewatcher.cpp
)

# The Dolphin entry the installer also uses, so the app can add it back when asked.
qt_add_resources(kuickshare servicemenu
    PREFIX /data
    BASE ${CMAKE_CURRENT_SOURCE_DIR}/../../data
    FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/kuickshare-send.desktop)

# The application menu entry and its icons, written when the AppImage is integrated.
set(INTEGRATION_FILES
    ${CMAKE_CURRENT_SOURCE_DIR}/../../data/io.github.the_beper.KuickShare.desktop
    ${CMAKE_CURRENT_SOURCE_DIR}/../../data/icons/hicolor/scalable/apps/io.github.the_beper.KuickShare.svg
    ${CMAKE_CURRENT_SOURCE_DIR}/../../data/icons/hicolor/symbolic/apps/io.github.the_beper.KuickShare-symbolic.svg)
qt_add_resources(kuickshare integration
    PREFIX /data
    BASE ${CMAKE_CURRENT_SOURCE_DIR}/../../data
    FILES ${INTEGRATION_FILES})

# QML files in subdirectories need a qmldir each: not the case here, so use the new behavior (silences QTP0004).
qt_policy(SET QTP0004 NEW)

qt_add_qml_module(kuickshare
    URI io.github.the_beper.KuickShare
    RESOURCE_PREFIX /qt/qml
    NO_CACHEGEN
    # The shell is not linked statically: its QML modules are found at run time, so nothing needs the plugin scan.
    NO_IMPORT_SCAN
    QML_FILES
        qml/Main.qml
        qml/TransfersPage.qml
        qml/SendPage.qml
        qml/SettingsPage.qml
        qml/HelpPage.qml
        qml/AboutDialog.qml
        qml/ShortcutsDialog.qml
        qml/SectionTitle.qml
        qml/MessageBox.qml
        qml/ToastStack.qml
        qml/PairDialog.qml
        qml/IntegrationDialog.qml
)

target_link_libraries(kuickshare PRIVATE
    Qt6::Core Qt6::Gui Qt6::Qml Qt6::Quick Qt6::QuickControls2 Qt6::DBus Qt6::Widgets
    KF6::Kirigami KF6::I18n KF6::I18nQml KF6::CoreAddons KF6::Crash KF6::DBusAddons KF6::StatusNotifierItem KF6::Notifications KF6::JobWidgets
)

# Qt 6.12+ (Arch) refuses executables that copy-relocate its data symbols.
target_compile_options(kuickshare PRIVATE -mno-direct-extern-access)

install(TARGETS kuickshare ${KDE_INSTALL_TARGETS_DEFAULT_ARGS})
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/kuickshare.notifyrc DESTINATION ${KDE_INSTALL_KNOTIFYRCDIR})
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/io.github.the_beper.KuickShare.desktop DESTINATION ${KDE_INSTALL_APPDIR})
# Dolphin ignores service menus that are not executable.
install(PROGRAMS ${CMAKE_CURRENT_SOURCE_DIR}/../../data/kuickshare-send.desktop DESTINATION ${KDE_INSTALL_DATADIR}/kio/servicemenus)
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/icons/hicolor/scalable/apps/io.github.the_beper.KuickShare.svg DESTINATION ${KDE_INSTALL_ICONDIR}/hicolor/scalable/apps)
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/icons/hicolor/symbolic/apps/io.github.the_beper.KuickShare-symbolic.svg DESTINATION ${KDE_INSTALL_ICONDIR}/hicolor/symbolic/apps)

file(CREATE_LINK ${CMAKE_CURRENT_SOURCE_DIR}/../../target/debug/kuickshared ${CMAKE_BINARY_DIR}/kuickshared SYMBOLIC)

enable_testing()
qt_add_executable(transfermodel_test ../../tests/shell/transfermodel_test.cpp transfermodel.cpp)
target_include_directories(transfermodel_test PRIVATE .)
target_link_libraries(transfermodel_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME transfermodel COMMAND transfermodel_test)

qt_add_executable(openablefile_test ../../tests/shell/openablefile_test.cpp)
target_include_directories(openablefile_test PRIVATE .)
target_link_libraries(openablefile_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME openablefile COMMAND openablefile_test)

qt_add_executable(folderexpand_test ../../tests/shell/folderexpand_test.cpp)
target_include_directories(folderexpand_test PRIVATE .)
target_link_libraries(folderexpand_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME folderexpand COMMAND folderexpand_test)

qt_add_executable(devicematch_test ../../tests/shell/devicematch_test.cpp)
target_include_directories(devicematch_test PRIVATE .)
target_link_libraries(devicematch_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME devicematch COMMAND devicematch_test)

qt_add_executable(transferfiltermodel_test ../../tests/shell/transferfiltermodel_test.cpp transferfiltermodel.cpp transfermodel.cpp)
target_include_directories(transferfiltermodel_test PRIVATE .)
target_link_libraries(transferfiltermodel_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME transferfiltermodel COMMAND transferfiltermodel_test)

qt_add_executable(devicemodel_test ../../tests/shell/devicemodel_test.cpp devicemodel.cpp)
target_include_directories(devicemodel_test PRIVATE .)
target_link_libraries(devicemodel_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME devicemodel COMMAND devicemodel_test)

qt_add_executable(dolphinmenu_test ../../tests/shell/dolphinmenu_test.cpp dolphinmenu.cpp)
qt_add_resources(dolphinmenu_test servicemenu
    PREFIX /data
    BASE ${CMAKE_CURRENT_SOURCE_DIR}/../../data
    FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/kuickshare-send.desktop)
target_include_directories(dolphinmenu_test PRIVATE .)
target_link_libraries(dolphinmenu_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME dolphinmenu COMMAND dolphinmenu_test)

qt_add_executable(execquote_test ../../tests/shell/execquote_test.cpp)
target_include_directories(execquote_test PRIVATE .)
target_link_libraries(execquote_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME execquote COMMAND execquote_test)

qt_add_executable(launcher_test ../../tests/shell/launcher_test.cpp)
target_include_directories(launcher_test PRIVATE .)
target_link_libraries(launcher_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME launcher COMMAND launcher_test)

qt_add_executable(integration_test ../../tests/shell/integration_test.cpp integration.cpp dolphinmenu.cpp)
qt_add_resources(integration_test servicemenu
    PREFIX /data
    BASE ${CMAKE_CURRENT_SOURCE_DIR}/../../data
    FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/kuickshare-send.desktop)
qt_add_resources(integration_test integration
    PREFIX /data
    BASE ${CMAKE_CURRENT_SOURCE_DIR}/../../data
    FILES ${INTEGRATION_FILES})
target_include_directories(integration_test PRIVATE .)
target_link_libraries(integration_test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME integration COMMAND integration_test)

qt_add_executable(instance_test ../../tests/shell/instance_test.cpp instance.cpp updatewatcher.cpp)
target_include_directories(instance_test PRIVATE .)
target_link_libraries(instance_test PRIVATE Qt6::Core Qt6::DBus Qt6::Test)
add_test(NAME instance COMMAND instance_test)
