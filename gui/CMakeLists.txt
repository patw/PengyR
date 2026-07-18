cmake_minimum_required(VERSION 3.16)
project(Pengy VERSION 1.3.10 LANGUAGES CXX)

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

find_package(Qt6 REQUIRED COMPONENTS Core Widgets Network)

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
      Qt6::Core Qt6::Widgets Qt6::Network
      ${PENGY_CORE_LIB}
      ws2_32 userenv ntdll bcrypt advapi32
    )
  elseif(APPLE)
    find_package(OpenSSL REQUIRED)
    target_link_libraries(pengy PRIVATE
      Qt6::Core Qt6::Widgets Qt6::Network
      ${PENGY_CORE_LIB}
      OpenSSL::SSL OpenSSL::Crypto
      "-framework Security" "-framework CoreFoundation" "-framework SystemConfiguration"
      pthread dl
    )
  else()
    find_package(OpenSSL REQUIRED)
    target_link_libraries(pengy PRIVATE
      Qt6::Core Qt6::Widgets Qt6::Network
      ${PENGY_CORE_LIB}
      OpenSSL::SSL OpenSSL::Crypto
      pthread dl m
    )
  endif()
else()
  target_link_libraries(pengy PRIVATE
    Qt6::Core Qt6::Widgets Qt6::Network
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
  target_link_libraries(pengy_chatview_markdown_test PRIVATE Qt6::Core Qt6::Widgets Qt6::Network)
  target_include_directories(pengy_chatview_markdown_test PRIVATE ${CMAKE_SOURCE_DIR})
  add_test(NAME pengy_chatview_markdown_test COMMAND pengy_chatview_markdown_test)
endif()
