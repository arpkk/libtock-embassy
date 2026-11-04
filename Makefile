SHELL := /usr/bin/env bash

# libtock-rs unfortunately currently uses Make. IMO we should migrate this to fully cargo based via features 
# and using runners like probe-rs similar to how embassy does this. I'd like cargo run --features "board name" to 
# handle everything from building + flashing. For now, we will use the existing hardcoded make commands to keep things easy.

PLATFORM := nrf52840
TARGET := thumbv7em-none-eabi
APP_ARCH := cortex-m4
EXAMPLE ?= blink
# Features de cargo separadas por coma. Los ejemplos que las exigen las activan solos.
FEATURES ?= $(if $(filter 6_tormenta_lenta 7_dos_procesos 8_mismo_periodo,$(EXAMPLE)),stats)
TARGET_DIR := target/$(PLATFORM)
ELF := $(TARGET_DIR)/$(TARGET)/release/examples/$(EXAMPLE)
TAB_DIR := target/tab
TAB := $(TAB_DIR)/$(EXAMPLE).tab
PROTECTED_REGION_SIZE ?= 128
# Flags de tockloader. La DK habla por JLink, no por bootloader serial.
TOCKLOADER_FLAGS ?= --jlink --board nrf52dk
LISTEN_PORT ?= /dev/cu.usbmodem0010502719211

.PHONY: all build tab flash listen flash-dos

# Segunda copia de la app para `flash-dos`. La primera usa las direcciones de
# PLATFORM (flash 0x40000, RAM 0x20010000); la segunda queda más arriba en ambas.
B_FLASH ?= 0x00050000
B_RAM ?= 0x20020000

# Keep default behavior equivalent to prior `make all`.
all: build

build:
	LIBTOCK_PLATFORM=$(PLATFORM) cargo build --example $(EXAMPLE) --release $(if $(FEATURES),--features $(FEATURES)) --target=$(TARGET) --target-dir=$(TARGET_DIR)

tab: build
	mkdir -p $(TAB_DIR)
	elf2tab --kernel-major 2 --kernel-minor 1 -n $(EXAMPLE) -o $(TAB) --stack 1024 --minimum-footer-size 256 --protected-region-size $(PROTECTED_REGION_SIZE) $(ELF),$(APP_ARCH)

# La app queda linkeada a una dirección fija, así que hay que borrar las que haya
# antes de instalar. `erase-apps` borra todas sin preguntar; `uninstall` sin nombre
# abre un menú cuando hay más de una y no borra nada.
flash: tab
	tockloader erase-apps $(TOCKLOADER_FLAGS)
	tockloader install $(TOCKLOADER_FLAGS) $(TAB)

listen:
	tockloader listen --port $(LISTEN_PORT)

# Instala dos copias del ejemplo como procesos separados, `<EXAMPLE>_a` y
# `<EXAMPLE>_b`. Cada una se compila con PROC=a o PROC=b en su propio target dir.
flash-dos:
	mkdir -p $(TAB_DIR)
	PROC=a LIBTOCK_PLATFORM=$(PLATFORM) cargo build --example $(EXAMPLE) --release $(if $(FEATURES),--features $(FEATURES)) --target=$(TARGET) --target-dir=$(TARGET_DIR)-a
	PROC=b LIBTOCK_LINKER_FLASH=$(B_FLASH) LIBTOCK_LINKER_FLASH_LENGTH=0x10000 LIBTOCK_LINKER_RAM=$(B_RAM) LIBTOCK_LINKER_RAM_LENGTH=64K cargo build --example $(EXAMPLE) --release $(if $(FEATURES),--features $(FEATURES)) --target=$(TARGET) --target-dir=$(TARGET_DIR)-b
	elf2tab --kernel-major 2 --kernel-minor 1 -n $(EXAMPLE)_a -o $(TAB_DIR)/$(EXAMPLE)_a.tab --stack 1024 --minimum-footer-size 256 --protected-region-size $(PROTECTED_REGION_SIZE) $(TARGET_DIR)-a/$(TARGET)/release/examples/$(EXAMPLE),$(APP_ARCH)
	elf2tab --kernel-major 2 --kernel-minor 1 -n $(EXAMPLE)_b -o $(TAB_DIR)/$(EXAMPLE)_b.tab --stack 1024 --minimum-footer-size 256 --protected-region-size $(PROTECTED_REGION_SIZE) $(TARGET_DIR)-b/$(TARGET)/release/examples/$(EXAMPLE),$(APP_ARCH)
	tockloader erase-apps $(TOCKLOADER_FLAGS)
	tockloader install $(TOCKLOADER_FLAGS) $(TAB_DIR)/$(EXAMPLE)_a.tab
	tockloader install $(TOCKLOADER_FLAGS) $(TAB_DIR)/$(EXAMPLE)_b.tab
