cmake_minimum_required(VERSION 3.16)

# Read the version from the root Cargo.toml's [workspace.package] section
# instead of hardcoding it here, so pengy_core/pengy-cli/pengy-web and this
# GUI never drift apart — bump the version in one place (Cargo.toml).
file(STRINGS "${CMAKE_CURRENT_SOURCE_DIR}/../Cargo.toml" PENGY_CARGO_VERSION_LINE REGEX "^version = ")
string(REGEX REPLACE "^version = \"([^\"]+)\".*" "\\1" PENGY_VERSION_FULL "${PENGY_CARGO_VERSION_LINE}")
# CMake's project(VERSION) only accepts plain dotted-numeric versions (no
# semver pre-release/build suffixes like "-beta"), so strip any such suffix
# here; Cargo itself still sees the full string for pengy_core/cli/web.
string(REGEX MATCH "^[0-9]+\\.[0-9]+\\.[0-9]+" PENGY_VERSION_STRING "${PENGY_VERSION_FULL}")

project(Pengy VERSION ${PENGY_VERSION_STRING} LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)
set(CMAKE_AUTORCC ON)

# Generate version.h from template
configure_file(
    ${CMAKE_SOURCE_DIR}/version.h.in
    ${CMAKE_BINARY_DIR}/version.h
    @ONLY
)

# Platform detection
if(APPLE)
  set(PENGY_PLATFORM "macOS")
elseif(WIN32)
  set(PENGY_PLATFORM "Windows")
else()
  set(PENGY_PLATFORM "Linux")
endif()
message(STATUS "Platform: ${PENGY_PLATFORM}")

find_package(Qt6 REQUIRED COMPONENTS Core Widgets Network Svg)

# ── Find Rust library ─────────────────────────────────────────────

set(RUST_TARGET_DIR "${RUST_TARGET_DIR}" CACHE PATH "Directory containing libpengy_core")
if(NOT RUST_TARGET_DIR)
  if(APPLE)
    execute_process(COMMAND uname -m OUTPUT_VARIABLE HOST_ARCH OUTPUT_STRIP_TRAILING_WHITESPACE)
    set(RUST_TARGET_DIR "${CMAKE_SOURCE_DIR}/../target/${HOST_ARCH}-apple-darwin/release")
  elseif(WIN32)
    set(RUST_TARGET_DIR "${CMAKE_SOURCE_DIR}/../target/x86_64-pc-windows-msvc/release")
  else()
    set(RUST_TARGET_DIR "${CMAKE_SOURCE_DIR}/../target/release")
  endif()
endif()

set(PENGY_CORE_LIB "")
set(PENGY_RUST_STATIC OFF)

# Try static first (.a or .lib), then shared (.so or .dylib or .dll)
if(WIN32)
  if(EXISTS "${RUST_TARGET_DIR}/pengy_core.lib")
    set(PENGY_CORE_LIB "${RUST_TARGET_DIR}/pengy_core.lib")
    set(PENGY_RUST_STATIC ON)
  elseif(EXISTS "${RUST_TARGET_DIR}/pengy_core.dll.lib")
    set(PENGY_CORE_LIB "${RUST_TARGET_DIR}/pengy_core.dll.lib")
  endif()
elseif(APPLE)
  if(EXISTS "${RUST_TARGET_DIR}/libpengy_core.a")
    set(PENGY_CORE_LIB "${RUST_TARGET_DIR}/libpengy_core.a")
    set(PENGY_RUST_STATIC ON)
  elseif(EXISTS "${RUST_TARGET_DIR}/libpengy_core.dylib")
    set(PENGY_CORE_LIB "${RUST_TARGET_DIR}/libpengy_core.dylib")
  endif()
else()
  if(EXISTS "${RUST_TARGET_DIR}/libpengy_core.a")
    set(PENGY_CORE_LIB "${RUST_TARGET_DIR}/libpengy_core.a")
    set(PENGY_RUST_STATIC ON)
  elseif(EXISTS "${RUST_TARGET_DIR}/libpengy_core.so")
    set(PENGY_CORE_LIB "${RUST_TARGET_DIR}/libpengy_core.so")
  endif()
endif()

if(NOT PENGY_CORE_LIB)
  message(FATAL_ERROR "Could not find pengy_core library in ${RUST_TARGET_DIR}. Build Rust first: cd .. && cargo build --release")
endif()

message(STATUS "Pengy core: ${PENGY_CORE_LIB} (static=${PENGY_RUST_STATIC})")

# ── Source files ──────────────────────────────────────────────────

add_executable(pengy
  main.cpp mainwindow.cpp chathistory.cpp chatview.cpp
  chatinput.cpp chatworker.cpp settingsdialog.cpp tasksdialog.cpp
  resources.qrc
)

# ── Link ──────────────────────────────────────────────────────────

if(PENGY_RUST_STATIC)
  if(WIN32)
    target_link_libraries(pengy PRIVATE
      Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg
      ${PENGY_CORE_LIB}
      ws2_32 userenv ntdll bcrypt advapi32
    )
  elseif(APPLE)
    find_package(OpenSSL REQUIRED)
    target_link_libraries(pengy PRIVATE
      Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg
      ${PENGY_CORE_LIB}
      "${OPENSSL_ROOT_DIR}/lib/libssl.dylib"
      "${OPENSSL_ROOT_DIR}/lib/libcrypto.dylib"
      "-framework Security" "-framework CoreFoundation" "-framework SystemConfiguration"
      pthread dl
    )
  else()
    find_package(OpenSSL REQUIRED)
    target_link_libraries(pengy PRIVATE
      Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg
      ${PENGY_CORE_LIB}
      OpenSSL::SSL OpenSSL::Crypto
      pthread dl m
    )
  endif()
else()
  target_link_libraries(pengy PRIVATE
    Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg
    ${PENGY_CORE_LIB}
  )
endif()

target_include_directories(pengy PRIVATE ${CMAKE_BINARY_DIR})

target_include_directories(pengy PRIVATE ${CMAKE_SOURCE_DIR})

# ── RPATH (Linux only, for bundling) ──────────────────────────────
if(NOT APPLE AND NOT WIN32)
  set_target_properties(pengy PROPERTIES
    INSTALL_RPATH "$ORIGIN/../lib"
    BUILD_RPATH "$ORIGIN/../target/release:$ORIGIN/../target/debug"
  )
endif()

# ── Windows: copy Qt DLLs after build ─────────────────────────────
if(WIN32)
  add_custom_command(TARGET pengy POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E echo "Run windeployqt pengy.exe to bundle Qt DLLs"
  )
endif()


# ── Optional GUI unit tests ───────────────────────────────────────
option(PENGY_BUILD_TESTS "Build lightweight Qt GUI tests" OFF)
if(PENGY_BUILD_TESTS)
  enable_testing()
  add_executable(pengy_chatview_markdown_test
    tests/test_chatview_markdown.cpp chatview.cpp
  )
  target_compile_definitions(pengy_chatview_markdown_test PRIVATE PENGY_UNIT_TEST)
  target_link_libraries(pengy_chatview_markdown_test PRIVATE Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg)
  target_include_directories(pengy_chatview_markdown_test PRIVATE ${CMAKE_SOURCE_DIR})
  add_test(NAME pengy_chatview_markdown_test COMMAND pengy_chatview_markdown_test)

  # chathistory.cpp calls into the Rust core (pengy_ffi.h), so this test
  # target needs the same platform-specific link set as the main `pengy`
  # target above, not just Qt.
  add_executable(pengy_chathistory_test
    tests/test_chathistory.cpp chathistory.cpp
  )
  target_compile_definitions(pengy_chathistory_test PRIVATE PENGY_UNIT_TEST)
  if(PENGY_RUST_STATIC)
    if(WIN32)
      target_link_libraries(pengy_chathistory_test PRIVATE
        Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg
        ${PENGY_CORE_LIB}
        ws2_32 userenv ntdll bcrypt advapi32
      )
    elseif(APPLE)
      target_link_libraries(pengy_chathistory_test PRIVATE
        Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg
        ${PENGY_CORE_LIB}
        "${OPENSSL_ROOT_DIR}/lib/libssl.dylib"
        "${OPENSSL_ROOT_DIR}/lib/libcrypto.dylib"
        "-framework Security" "-framework CoreFoundation" "-framework SystemConfiguration"
        pthread dl
      )
    else()
      target_link_libraries(pengy_chathistory_test PRIVATE
        Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg
        ${PENGY_CORE_LIB}
        OpenSSL::SSL OpenSSL::Crypto
        pthread dl m
      )
    endif()
  else()
    target_link_libraries(pengy_chathistory_test PRIVATE
      Qt6::Core Qt6::Widgets Qt6::Network Qt6::Svg
      ${PENGY_CORE_LIB}
    )
  endif()
  target_include_directories(pengy_chathistory_test PRIVATE ${CMAKE_SOURCE_DIR})
  add_test(NAME pengy_chathistory_test COMMAND pengy_chathistory_test)
endif()
