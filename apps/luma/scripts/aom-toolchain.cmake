# Portable codec build without installing NASM. Native SIMD C remains enabled.
set(ENABLE_NASM OFF CACHE BOOL "" FORCE)
set(CONFIG_AV1_ENCODER 0 CACHE STRING "" FORCE)
set(AOM_TARGET_CPU generic CACHE STRING "" FORCE)
if(EXISTS "C:/Program Files/Git/usr/bin/perl.exe")
  set(PERL_EXECUTABLE "C:/Program Files/Git/usr/bin/perl.exe" CACHE FILEPATH "" FORCE)
endif()
