cmake_minimum_required(VERSION 3.24)
project(kuickshare VERSION 1.7.1 LANGUAGES CXX)
# Defines BUILD_TESTING (ON by default): packages pass -DBUILD_TESTING=OFF, build/check.sh -DBUILD_TESTING=ON.
include(CTest)
add_compile_definitions(KUICKSHARE_VERSION="${PROJECT_VERSION}")

find_package(ECM REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

# Qt 6 must be decided before KDEInstallDirs: with it unset until the find_package(Qt6) below, the
# module includes its KF5 variant and installs the notifyrc into knotifications5, a name no KF6
# runtime reads (Fedora 45 and Ubuntu 26.04 ship knotifications6, both libraries carry the name).
set(QT_MAJOR_VERSION 6)
include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Path to the kuickshared cargo produced; packages and build-host-payload.sh pass it (absolute or relative
# to the source root), dev builds leave it empty and keep the target/debug link below.
set(KUICKSHARED_PATH "" CACHE FILEPATH "Built kuickshared, installed next to the shell (system packages)")

find_package(Qt6 6.9 REQUIRED COMPONENTS Core Gui Qml Quick QuickControls2 DBus Widgets)
find_package(KF6 REQUIRED COMPONENTS Kirigami I18n CoreAddons Crash DBusAddons StatusNotifierItem Notifications JobWidgets)
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
        qml/WifiRequirementsDialog.qml
)

target_link_libraries(kuickshare PRIVATE
    Qt6::Core Qt6::Gui Qt6::Qml Qt6::Quick Qt6::QuickControls2 Qt6::DBus Qt6::Widgets
    KF6::Kirigami KF6::I18n KF6::I18nQml KF6::CoreAddons KF6::Crash KF6::DBusAddons KF6::StatusNotifierItem KF6::Notifications KF6::JobWidgets
)

# Qt 6.12+ (Arch) refuses executables that copy-relocate its data symbols.
target_compile_options(kuickshare PRIVATE -mno-direct-extern-access)

# The following install rules are what a system package gets with one `cmake --install --prefix /usr`:
# build/build-host-payload.sh stages its tarball payload from the same set (every file the payload
# needs must be listed here), a relative KUICKSHARED_PATH is resolved against the source root.
install(TARGETS kuickshare ${KDE_INSTALL_TARGETS_DEFAULT_ARGS})
if(KUICKSHARED_PATH)
    # The daemon cargo builds, installed next to the shell: corebridge.cpp starts it from applicationDirPath().
    install(PROGRAMS "${KUICKSHARED_PATH}" DESTINATION ${KDE_INSTALL_BINDIR})
endif()
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/kuickshare.notifyrc DESTINATION ${KDE_INSTALL_KNOTIFYRCDIR})
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/io.github.the_beper.KuickShare.desktop DESTINATION ${KDE_INSTALL_APPDIR})
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/io.github.the_beper.KuickShare.metainfo.xml DESTINATION ${KDE_INSTALL_METAINFODIR})
# Dolphin ignores service menus that are not executable.
install(PROGRAMS ${CMAKE_CURRENT_SOURCE_DIR}/../../data/kuickshare-send.desktop DESTINATION ${KDE_INSTALL_DATADIR}/kio/servicemenus)
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/icons/hicolor/scalable/apps/io.github.the_beper.KuickShare.svg DESTINATION ${KDE_INSTALL_ICONDIR}/hicolor/scalable/apps)
install(FILES ${CMAKE_CURRENT_SOURCE_DIR}/../../data/icons/hicolor/symbolic/apps/io.github.the_beper.KuickShare-symbolic.svg DESTINATION ${KDE_INSTALL_ICONDIR}/hicolor/symbolic/apps)

# Translations: one catalog per language in po/LINGUAS, next to the KDE ones (/usr/share/locale).
find_program(GETTEXT_MSGFMT_EXECUTABLE msgfmt)
if(GETTEXT_MSGFMT_EXECUTABLE)
    file(READ ${CMAKE_CURRENT_SOURCE_DIR}/../../po/LINGUAS linguas)
    string(REPLACE "\n" ";" langs "${linguas}")
    set(mo_files "")
    foreach(lang ${langs})
        if(lang STREQUAL "")
            continue()
        endif()
        set(mo ${CMAKE_CURRENT_BINARY_DIR}/locale/${lang}/LC_MESSAGES/kuickshare.mo)
        add_custom_command(OUTPUT ${mo}
            COMMAND ${GETTEXT_MSGFMT_EXECUTABLE} --check-format -o ${mo} ${CMAKE_CURRENT_SOURCE_DIR}/../../po/${lang}.po
            DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/../../po/${lang}.po)
        list(APPEND mo_files ${mo})
        install(FILES ${mo} DESTINATION ${KDE_INSTALL_LOCALEDIR}/${lang}/LC_MESSAGES)
    endforeach()
    add_custom_target(kuickshare-mo ALL DEPENDS ${mo_files})
    if(NOT langs)
        message(FATAL_ERROR "po/LINGUAS is empty")
    endif()
else()
    message(STATUS "msgfmt not found: skipping the .mo catalogs")
endif()

file(CREATE_LINK ${CMAKE_CURRENT_SOURCE_DIR}/../../target/debug/kuickshared ${CMAKE_BINARY_DIR}/kuickshared SYMBOLIC)

if(BUILD_TESTING)
    find_package(Qt6 REQUIRED COMPONENTS Test)

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

    qt_add_executable(requirementsreport_test ../../tests/shell/requirementsreport_test.cpp)
    target_include_directories(requirementsreport_test PRIVATE .)
    target_link_libraries(requirementsreport_test PRIVATE Qt6::Core Qt6::Test)
    add_test(NAME requirementsreport COMMAND requirementsreport_test)
endif()
