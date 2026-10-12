# Forces the static MSVC CRT (/MT) for every CMake-built dependency (whisper.cpp, CTranslate2),
# matching Rust's +crt-static. whisper-rs-sys always builds the "Release" config, whose default
# /MD flags cmake-rs doesn't override in dev builds, causing LNK2038 CRT mismatches.
# Runs as both the toolchain file and (via CMAKE_PROJECT_INCLUDE) after each project() call,
# where the per-config default flags already exist and can be rewritten.
if(CMAKE_HOST_WIN32)
  set(CMAKE_PROJECT_INCLUDE "${CMAKE_CURRENT_LIST_FILE}")
  set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded")
  foreach(lang C CXX)
    foreach(cfg "" _DEBUG _RELEASE _RELWITHDEBINFO _MINSIZEREL)
      if(DEFINED CMAKE_${lang}_FLAGS${cfg})
        string(REGEX REPLACE "[/-]MDd?( |$)" "/MT\\1" _f "${CMAKE_${lang}_FLAGS${cfg}}")
        set(CMAKE_${lang}_FLAGS${cfg} "${_f}" CACHE STRING "" FORCE)
      endif()
    endforeach()
    # In cargo --release builds cmake-rs replaces CMAKE_<LANG>_FLAGS_RELEASE with cc's flags,
    # which carry no optimisation level for MSVC, so whisper.cpp and CTranslate2 were built
    # unoptimised (whisper ran ~5x slower than in `tauri dev`). Put /O2 back if it's missing.
    if(DEFINED CMAKE_${lang}_FLAGS_RELEASE AND NOT CMAKE_${lang}_FLAGS_RELEASE MATCHES "[/-]O[12xd]")
      set(CMAKE_${lang}_FLAGS_RELEASE "${CMAKE_${lang}_FLAGS_RELEASE} /O2 /Ob2 /DNDEBUG" CACHE STRING "" FORCE)
    endif()
  endforeach()
endif()
