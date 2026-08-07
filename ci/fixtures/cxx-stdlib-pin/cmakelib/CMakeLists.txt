cmake_minimum_required(VERSION 3.15)
project(pinlib CXX)

add_library(pinlib STATIC pinlib.cpp)
install(TARGETS pinlib DESTINATION lib)
