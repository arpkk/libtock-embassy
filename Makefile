SHELL := /usr/bin/env bash

# libtock-rs unfortunately currently uses Make. IMO we should migrate this to fully cargo based via features 
# and using runners like probe-rs similar to how embassy does this. I'd like cargo run --features "board name" to 
# handle everything from building + flashing. For now, we will use the existing hardcoded make commands to keep things easy.

PLATFORM := nrf52840
TARGET := thumbv7em-none-eabi
APP_ARCH := cortex-m4
EXAMPLE ?= blink
TARGET_DIR := target/$(PLATFORM)
ELF := $(TARGET_DIR)/$(TARGET)/release/examples/$(EXAMPLE)
TAB_DIR := target/tab
TAB := $(TAB_DIR)/$(EXAMPLE).tab
PROTECTED_REGION_SIZE ?= 128
# Flags de tockloader. La DK habla por JLink, no por bootloader serial.
TOCKLOADER_FLAGS ?= --jlink --board nrf52dk
LISTEN_PORT ?= /dev/cu.usbmodem0010502719211

.PHONY: all build tab flash listen

# Keep default behavior equivalent to prior `make all`.
all: build

build:
	LIBTOCK_PLATFORM=$(PLATFORM) cargo build --example $(EXAMPLE) --release --target=$(TARGET) --target-dir=$(TARGET_DIR)

tab: build
	mkdir -p $(TAB_DIR)
	elf2tab --kernel-major 2 --kernel-minor 1 -n $(EXAMPLE) -o $(TAB) --stack 1024 --minimum-footer-size 256 --protected-region-size $(PROTECTED_REGION_SIZE) $(ELF),$(APP_ARCH)

# La app queda linkeada a una dirección fija, así que solo cabe una: hay que
# desinstalar antes de instalar (el `|| true` es para cuando la placa ya está vacía).
flash: tab
	tockloader uninstall $(TOCKLOADER_FLAGS) || true
	tockloader install $(TOCKLOADER_FLAGS) $(TAB)

listen:
	tockloader listen --port $(LISTEN_PORT)
