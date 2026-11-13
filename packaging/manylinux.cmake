# CMake toolchain file for packaging/build-linux.sh (passed as CMAKE_TOOLCHAIN_FILE).
# The manylinux images are RHEL-family, where GNUInstallDirs installs libraries to <prefix>/lib64;
# charls-sys only adds <prefix>/lib to the link search path, so linking fails with -lcharls not
# found. Install to lib instead. Compiler selection is unchanged (the image's gcc-toolset).
set(CMAKE_INSTALL_LIBDIR lib CACHE PATH "Object code libraries")
