#!/usr/bin/env bash
# Comprueba el entorno de Yeison y instala, en orden, solo lo que falte.
# En macOS hace falta: herramientas de Xcode (clang, git y el SDK), curl y Rust 1.85 o más nuevo.

set -u

MIN_RUST="1.85.0"
CARGO_ENV="${HOME}/.cargo/env"

if [[ -f "${CARGO_ENV}" ]]; then
  # shellcheck disable=SC1090
  source "${CARGO_ENV}"
fi

if [[ -d "${HOME}/.cargo/bin" ]]; then
  export PATH="${HOME}/.cargo/bin:${PATH}"
fi

bold() {
  if [[ -t 1 ]]; then
    printf '\033[1m%s\033[0m\n' "$1"
  else
    printf '%s\n' "$1"
  fi
}

have() {
  command -v "$1" >/dev/null 2>&1
}

version_ge() {
  # Verdadero cuando $1 es mayor o igual que $2 (versiones numéricas a.b.c).
  awk -v have="$1" -v need="$2" 'BEGIN {
    split(have, H, ".");
    split(need, N, ".");
    for (i = 1; i <= 3; i++) {
      hi = H[i] + 0;
      ni = N[i] + 0;
      if (hi > ni) exit 0;
      if (hi < ni) exit 1;
    }
    exit 0;
  }'
}

rustc_version() {
  rustc --version 2>/dev/null | awk '{print $2}'
}

xcode_ready() {
  xcode-select -p >/dev/null 2>&1 && have clang && have git
}

rust_ready() {
  have cargo && have rustc && version_ge "$(rustc_version)" "${MIN_RUST}"
}

refresh_rust_path() {
  if [[ -f "${CARGO_ENV}" ]]; then
    # shellcheck disable=SC1090
    source "${CARGO_ENV}"
  fi
  if [[ -d "${HOME}/.cargo/bin" ]]; then
    export PATH="${HOME}/.cargo/bin:${PATH}"
  fi
  hash -r
}

# --- Estado inicial -------------------------------------------------------

os_ok=0
xcode_ok=0
curl_ok=0
rust_ok=0

[[ "$(uname -s)" == "Darwin" ]] && os_ok=1
xcode_ready && xcode_ok=1
have curl && curl_ok=1
rust_ready && rust_ok=1

bold "Comprobación del entorno"
echo
printf '  %-44s %s\n' "macOS" "$([[ ${os_ok} -eq 1 ]] && echo listo || echo falta)"
printf '  %-44s %s\n' "Herramientas de Xcode (clang, git, SDK)" "$([[ ${xcode_ok} -eq 1 ]] && echo listo || echo falta)"
printf '  %-44s %s\n' "curl" "$([[ ${curl_ok} -eq 1 ]] && echo listo || echo falta)"
if [[ ${rust_ok} -eq 1 ]]; then
  printf '  %-44s %s\n' "Rust $(rustc_version) y cargo" "listo"
else
  if have rustc; then
    printf '  %-44s %s\n' "Rust $(rustc_version) (hace falta ${MIN_RUST} o más nuevo)" "falta"
  else
    printf '  %-44s %s\n' "Rust ${MIN_RUST} o más nuevo, con cargo" "falta"
  fi
fi
echo

if [[ ${os_ok} -eq 1 && ${xcode_ok} -eq 1 && ${curl_ok} -eq 1 && ${rust_ok} -eq 1 ]]; then
  echo "Ya puede ejecutar el proyecto con el comando cargo run"
  exit 0
fi

if [[ ${os_ok} -eq 0 ]]; then
  echo "Yeison usa la isla del notch y AppKit, así que este instalador corre en macOS."
  exit 1
fi

bold "Falta instalar, en este orden"
echo
step=1
if [[ ${xcode_ok} -eq 0 ]]; then
  echo "  ${step}. Herramientas de línea de comandos de Xcode"
  step=$((step + 1))
fi
if [[ ${curl_ok} -eq 0 ]]; then
  echo "  ${step}. curl"
  step=$((step + 1))
fi
if [[ ${rust_ok} -eq 0 ]]; then
  echo "  ${step}. Rust (rustup, rustc y cargo)"
fi
echo

# --- 1. Xcode -------------------------------------------------------------

if [[ ${xcode_ok} -eq 0 ]]; then
  bold "Instalando las herramientas de Xcode"
  echo "Acepta la ventana de Apple si aparece. Este script espera a que terminen."
  echo "Puedes cancelar la espera con Ctrl+C y volver a ejecutar el script después."
  xcode-select --install >/dev/null 2>&1 || true

  waited=0
  until xcode_ready; do
    sleep 5
    waited=$((waited + 5))
    if [[ $((waited % 30)) -eq 0 ]]; then
      echo "  Sigue la instalación de Xcode… (${waited}s)"
    fi
    if [[ ${waited} -ge 1800 ]]; then
      echo "La instalación de Xcode no terminó. Cuando la ventana de Apple cierre, vuelve a ejecutar este script."
      exit 1
    fi
  done
  echo "  Herramientas de Xcode listas."
  echo
fi

# --- 2. curl --------------------------------------------------------------

if ! have curl; then
  echo "curl no está en el sistema y Rust se descarga con él."
  echo "Instala las herramientas de Xcode, o curl, y vuelve a ejecutar este script."
  exit 1
fi

# --- 3. Rust --------------------------------------------------------------

rust_installed_now=0
if ! rust_ready; then
  bold "Instalando Rust"
  refresh_rust_path
  if have rustup; then
    rustup toolchain install stable
    rustup default stable
  else
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  fi
  refresh_rust_path
  rust_installed_now=1
  echo
fi

# --- Cierre ---------------------------------------------------------------

xcode_ok=0
curl_ok=0
rust_ok=0
xcode_ready && xcode_ok=1
have curl && curl_ok=1
rust_ready && rust_ok=1

bold "Comprobación final"
echo
printf '  %-44s %s\n' "macOS" "listo"
printf '  %-44s %s\n' "Herramientas de Xcode (clang, git, SDK)" "$([[ ${xcode_ok} -eq 1 ]] && echo listo || echo falta)"
printf '  %-44s %s\n' "curl" "$([[ ${curl_ok} -eq 1 ]] && echo listo || echo falta)"
if [[ ${rust_ok} -eq 1 ]]; then
  printf '  %-44s %s\n' "Rust $(rustc_version) y cargo" "listo"
else
  printf '  %-44s %s\n' "Rust ${MIN_RUST} o más nuevo, con cargo" "falta"
fi
echo

if [[ ${xcode_ok} -eq 1 && ${curl_ok} -eq 1 && ${rust_ok} -eq 1 ]]; then
  if [[ ${rust_installed_now} -eq 1 ]]; then
    echo "Si esta terminal no reconoce cargo, abre una terminal nueva o ejecuta:"
    echo "  source \"${HOME}/.cargo/env\""
    echo
  fi
  echo "Ya puede ejecutar el proyecto con el comando cargo run"
  exit 0
fi

echo "Sigue faltando algo. Revisa los mensajes de arriba y vuelve a ejecutar este script."
exit 1
