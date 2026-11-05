cmake_minimum_required(VERSION 3.20)
project(teams-redir C)

find_package(PkgConfig REQUIRED)
pkg_check_modules(FREERDP REQUIRED IMPORTED_TARGET freerdp3 winpr3)
find_package(Threads REQUIRED)
find_program(CARGO cargo REQUIRED)
find_program(CBINDGEN cbindgen REQUIRED)

set(CARGO_TARGET_DIR ${CMAKE_BINARY_DIR}/cargo)
set(FFI_LIB ${CARGO_TARGET_DIR}/release/libteams_redir_ffi.a)
set(FFI_HEADER ${CMAKE_BINARY_DIR}/include/tr_core.h)
file(GLOB_RECURSE RUST_SOURCES CONFIGURE_DEPENDS ${CMAKE_SOURCE_DIR}/crates/*.rs ${CMAKE_SOURCE_DIR}/crates/*/Cargo.toml)

# The Rust core, as a static library.
add_custom_command(
  OUTPUT ${FFI_LIB}
  COMMAND ${CMAKE_COMMAND} -E env CARGO_TARGET_DIR=${CARGO_TARGET_DIR}
          ${CARGO} build --release --locked -p teams-redir-ffi
  WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
  DEPENDS ${RUST_SOURCES} ${CMAKE_SOURCE_DIR}/Cargo.toml ${CMAKE_SOURCE_DIR}/Cargo.lock
  COMMENT "cargo build teams-redir-ffi")

# The C header for it.
add_custom_command(
  OUTPUT ${FFI_HEADER}
  COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/include
  COMMAND ${CBINDGEN} --config ${CMAKE_SOURCE_DIR}/crates/ffi/cbindgen.toml
          --crate teams-redir-ffi --output ${FFI_HEADER} ${CMAKE_SOURCE_DIR}/crates/ffi
  DEPENDS ${RUST_SOURCES} ${CMAKE_SOURCE_DIR}/crates/ffi/cbindgen.toml
  COMMENT "cbindgen tr_core.h")
add_custom_target(teams_redir_rust DEPENDS ${FFI_LIB} ${FFI_HEADER})

# The helper that shows the call's video in windows. Optional: without SDL3 there is no video
# window, and audio calls are unaffected.
pkg_check_modules(SDL3 QUIET sdl3)
if(SDL3_FOUND)
  set(VIDEO_BIN ${CARGO_TARGET_DIR}/release/teams-redir-video)
  add_custom_command(
    OUTPUT ${VIDEO_BIN}
    COMMAND ${CMAKE_COMMAND} -E env CARGO_TARGET_DIR=${CARGO_TARGET_DIR}
            ${CARGO} build --release --locked -p teams-redir-video-window
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS ${RUST_SOURCES} ${CMAKE_SOURCE_DIR}/Cargo.toml ${CMAKE_SOURCE_DIR}/Cargo.lock
    COMMENT "cargo build teams-redir-video")
  add_custom_target(teams_redir_video ALL DEPENDS ${VIDEO_BIN})
  install(PROGRAMS ${VIDEO_BIN} DESTINATION lib/freerdp3)
else()
  message(WARNING "SDL3 not found: teams-redir-video (the video windows) will not be built")
endif()

# FreeRDP loads /dvc:teams-redir from lib<name>-client.so and calls DVCPluginEntry.
add_library(teams-redir-client MODULE shim/shim.c)
add_dependencies(teams-redir-client teams_redir_rust)
target_include_directories(teams-redir-client PRIVATE ${CMAKE_BINARY_DIR}/include)
target_compile_options(teams-redir-client PRIVATE -Wall -Wextra -fvisibility=hidden)
target_link_libraries(teams-redir-client PRIVATE PkgConfig::FREERDP
  ${FFI_LIB}
  Threads::Threads ${CMAKE_DL_LIBS} m)
# Export only the entry point: libwebrtc (phase 1.5) brings its own BoringSSL and abseil, and
# FreeRDP has OpenSSL in the same process.
target_link_options(teams-redir-client PRIVATE
  -Wl,--version-script=${CMAKE_SOURCE_DIR}/shim/teams-redir.map -Wl,--exclude-libs,ALL -Wl,--gc-sections)
set_target_properties(teams-redir-client PROPERTIES PREFIX "lib" C_STANDARD 11)

install(TARGETS teams-redir-client LIBRARY DESTINATION lib/freerdp3)

# Drives the built module through FreeRDP's DVC plugin interface with a mock channel manager.
enable_testing()
add_executable(shim_test tests/shim_test.c)
target_link_libraries(shim_test PRIVATE PkgConfig::FREERDP ${CMAKE_DL_LIBS})
add_dependencies(shim_test teams-redir-client)
add_test(NAME shim COMMAND ${CMAKE_COMMAND} -E env TEAMS_REDIR_STATE_DIR=${CMAKE_BINARY_DIR}/test-state
         TEAMS_REDIR_TRANSCRIPTS=off $<TARGET_FILE:shim_test> $<TARGET_FILE:teams-redir-client>)
