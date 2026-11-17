# LabDrop

**Distribuição rápida, portátil e temporária de arquivos para laboratórios de informática (Windows 10/11 e Linux).**

O **LabDrop** resolve um problema prático de laboratório: você precisa distribuir alguns instaladores, apostilas ou pastas para vários computadores de uma sala (rodando Windows 10/11, Fedora, Ubuntu ou Linux Mint) sem precisar instalar agentes, configurar servidores permanentes ou passar o pendrive de máquina em máquina.

Os computadores de destino **não precisam ter o LabDrop instalado** — eles acessam os arquivos diretamente pelo gerenciador de arquivos nativo (Windows Explorer / Linux Arquivos) ou pelo navegador web.

---

## Sumário

1. [Instalação Rápida (GitHub Releases)](#1-instalação-rápida-github-releases)
2. [Modos de Instalação: Portátil (Pendrive) vs. Local (Computador)](#2-modos-de-instalação-portátil-pendrive-vs-local-computador)
3. [Como Executar no Windows e no Linux](#3-como-executar-no-windows-e-no-linux)
4. [Como Atualizar o LabDrop](#4-como-atualizar-o-labdrop)
5. [Como Usar no Dia a Dia](#5-como-usar-no-dia-a-dia)
6. [Troca de Interface de Rede (Cabo / Wi-Fi) sem Fechar o App](#6-troca-de-interface-de-rede-cabo--wi-fi-sem-fechar-o-app)
7. [Como Acessar os Arquivos nos Computadores de Destino](#7-como-acessar-os-arquivos-nos-computadores-de-destino)
8. [Samba no Linux: Quando é Necessário e Como Instalar](#8-samba-no-linux-quando-é-necessário-e-como-instalar)
9. [Solução de Problemas Comuns (Troubleshooting)](#9-solução-de-problemas-comuns-troubleshooting)
10. [Segurança e Limpeza Automática](#10-segurança-e-limpeza-automática)
11. [Para Desenvolvedores (Compilação a partir do Código-Fonte)](#11-para-desenvolvedores-compilação-a-partir-do-código-fonte)

---

## 1. Instalação Rápida (GitHub Releases)

Para instalar ou atualizar o LabDrop, **você não precisa clonar o repositório nem instalar o Rust**. Basta baixar o **Bootstrapper** na página de [**Releases do GitHub**](https://github.com/4mrco/labdrop/releases/latest):

- **No Windows (10/11 x86-64):** baixe **`labdrop-bootstrapper.exe`**
- **No Linux (Fedora, Ubuntu, Linux Mint x86-64):** baixe **`labdrop-bootstrapper-linux-x86_64.tar.gz`** *(recomendado, pois já vem com permissão de execução)* ou o binário direto **`labdrop-bootstrapper`**.

O Bootstrapper se conecta automaticamente ao GitHub Releases, baixa os dois executáveis oficiais (`LabDrop.exe` para Windows e `labdrop` para Linux), verifica a integridade criptográfica (`SHA-256`) e prepara a estrutura pronta para uso.

---

## 2. Modos de Instalação: Portátil (Pendrive) vs. Local (Computador)

Ao abrir o `labdrop-bootstrapper`, você pode escolher entre dois modos de instalação:

### A. Instalação Portátil (Pendrive USB)
Ideal para levar no bolso entre diferentes laboratórios. O Bootstrapper cria a pasta `<SeuPendrive>/LabDrop/` contendo **ambos** os executáveis (`LabDrop.exe` e `labdrop`):
```text
<Pendrive>/
└── LabDrop/
    ├── LabDrop.exe      # Executável nativo para Windows 10/11 (x86-64)
    ├── labdrop          # Executável estático para Linux (Fedora, Ubuntu, Mint x86-64)
    ├── files/           # Pasta opcional para guardar seus instaladores/arquivos no pendrive
    ├── config/          # Configurações portáteis (labdrop.toml)
    └── SHA256SUMS.txt   # Hashes SHA-256 verificados
```

### B. Instalação Local (No seu Computador)
Ideal quando você transmite arquivos sempre a partir do seu próprio notebook ou desktop.
- Selecione a opção **`[Local PC Install] ~/LabDrop`** na lista do Bootstrapper (ou pressione **`[d]`** para informar qualquer pasta do seu computador).
- O Bootstrapper instalará o pacote diretamente em `<destino>/LabDrop/` com a mesma estrutura completa e verificada.

> **Nota importante:** O Bootstrapper **nunca formata** seu pendrive ou disco, nunca altera arquivos fora da pasta `LabDrop/`, e instala **apenas** o LabDrop (nunca instala pacotes do sistema como o Samba automaticamente).

---

## 3. Como Executar no Windows e no Linux

### No Windows 10 / 11
1. Dê dois cliques em **`labdrop-bootstrapper.exe`** para instalar em um pendrive ou pasta local.
2. Para usar o programa, abra a pasta `LabDrop` criada e dê dois cliques em **`LabDrop.exe`**.
   - *(Opcional)* Para criar compartilhamentos SMB nativos do Windows na porta 445, clique com o botão direito em `LabDrop.exe` e escolha **Executar como administrador**.

### No Linux (Fedora, Ubuntu, Linux Mint)

#### Opção 1: Usando o arquivo `.tar.gz` (Mais simples)
O arquivo **`labdrop-bootstrapper-linux-x86_64.tar.gz`** já preserva a permissão de execução (`+x`):
1. Extraia o arquivo pelo gerenciador de arquivos (ou `tar -xzf labdrop-bootstrapper-linux-x86_64.tar.gz`).
2. Dê **dois cliques** em `labdrop-bootstrapper` (ele abrirá automaticamente uma janela de terminal com a interface gráfica de texto) ou execute `./labdrop-bootstrapper` no terminal.

#### Opção 2: Usando o binário direto `labdrop-bootstrapper`
Por padrão de segurança no Linux, navegadores salvam arquivos baixados na pasta `Downloads` **sem permissão de execução** (`rw-rw-r--`). Por isso:
- Executar `/home/usuario/Downloads/labdrop-bootstrapper` diretamente exibirá `Permission denied` (Permissão negada).
- Executar `sudo /home/usuario/Downloads/labdrop-bootstrapper` exibirá `sudo: ...: command not found`, porque o `sudo` se recusa a executar um arquivo que não possui o bit executável (`+x`).
- **Não execute o bootstrapper com `sudo`!** Ele deve rodar com seu usuário normal para que a pasta instalada pertença a você.

Basta conceder permissão de execução uma única vez no terminal:
```bash
chmod +x ~/Downloads/labdrop-bootstrapper
~/Downloads/labdrop-bootstrapper
```
Depois de instalado (por exemplo, em `~/LabDrop` ou no pendrive), inicie o LabDrop dando dois cliques no executável **`labdrop`** ou rodando no terminal:
```bash
~/LabDrop/labdrop
```

---

## 4. Como Atualizar o LabDrop

Para atualizar uma instalação existente (no pendrive ou no computador) para uma nova versão:
1. Execute o **`labdrop-bootstrapper`**.
2. Selecione o destino marcado como **`[UPDATE LabDrop]`** ou **`[UPDATE Local PC]`** e pressione **`Enter`** (ou clique em **Instalar / Atualizar**).
3. Os binários `labdrop` e `LabDrop.exe` serão atualizados e verificados via SHA-256, **preservando intactos** todos os seus arquivos dentro de `files/` e suas preferências salvas em `config/labdrop.toml`.

---

## 5. Como Usar no Dia a Dia

A interface do LabDrop funciona tanto pelo **teclado** quanto por **cliques do mouse**:

- **Barra de navegação superior (`[1]` a `[6]`):** mostra sempre em qual tela você está e permite alternar rapidamente entre:
  - **`[1] Início`**: Visão geral da sessão, resumo do sistema e ações rápidas.
  - **`[2] Arquivos`**: Navegador integrado de arquivos e pastas (pastas ocultas `.*` ficam ocultas por padrão e podem ser exibidas com `[.]`).
  - **`[3] Selecionados`**: Lista de revisão dos itens que serão compartilhados.
  - **`[4] Rede`**: Configuração da interface de rede preferida, protocolo e status do Samba.
  - **`[5] Compartilhamento`**: Painel da sessão ativa, seletor rápido de interface de rede e controle de reinício do serviço.
  - **`[6] Diagnóstico`**: Inspeção completa do sistema, firewall e recuperação de sessões órfãs.
  - **`[L] PT-BR / EN`**: Alterna o idioma da interface entre Português e Inglês a qualquer momento.

### Selecionando Arquivos e Pastas
Você pode adicionar arquivos e pastas de diferentes locais do computador ou do pendrive combinando qualquer um dos métodos abaixo:
- **Arrastar e Soltar (Drag & Drop):** arraste arquivos ou pastas do gerenciador de arquivos do sistema e solte-os diretamente sobre a janela do terminal do LabDrop.
- **Seletor Nativo do Sistema:** pressione **`[o]`** (escolher arquivos) ou **`[f]`** (escolher pasta) para abrir a janela nativa do sistema operacional.
- **Navegador Integrado (`[2] Arquivos`):** navegue com as setas ou roda do mouse, pressione **`Espaço`** (ou clique na caixa `[ ]`) para marcar arquivos/pastas e **`Enter`** para entrar em uma pasta.

> **Staging Seguro no Disco Interno:** Ao iniciar o compartilhamento, o LabDrop copia os itens selecionados para uma pasta temporária no armazenamento interno do computador transmissor (`/var/tmp` no Linux ou `%TEMP%` no Windows). Seus arquivos originais nunca são modificados e o pendrive não sofre leitura contínua durante os downloads.

---

## 6. Troca de Interface de Rede (Cabo / Wi-Fi) sem Fechar o App

Um cenário muito comum em laboratório: você abre o LabDrop conectado ao cabo Ethernet, mas depois conecta ao Wi-Fi do laboratório e quer transmitir os arquivos pela rede Wi-Fi sem fechar o aplicativo nem preparar os arquivos de novo.

### Como trocar de interface durante a sessão:
1. Abra a aba **`[5] Compartilhamento`** (clicando nela ou pressionando `5`).
2. No painel à direita (**Controles de Interface de Rede e Serviço**), você verá todas as interfaces detectadas com:
   - Nome da interface (`enp3s0`, `wlp2s0`, `Ethernet`, `Wi-Fi`)
   - Tipo de conexão (`Ethernet (Cabo)`, `Wi-Fi`, `Virtual`)
   - Status (`Conectada`)
   - Endereço IPv4 atribuído e indicação **`[ATIVA]`** na interface em uso no momento.
3. Se você acabou de conectar o Wi-Fi ou cabo, pressione **`[r]`** (ou clique em **Atualizar Rede**) para recarregar as interfaces.
4. Selecione a nova interface desejada (com as setas `↑/↓` ou clicando nela com o mouse).
5. Pressione **`[R]`** (ou clique em **`[R] Reiniciar Serviço`**).

O que o LabDrop faz automaticamente ao reiniciar o serviço:
- Encerra com segurança os ouvintes antigos, o processo Samba anterior e as regras temporárias de firewall da interface antiga.
- Reinicia o serviço de compartilhamento na nova interface selecionada reutilizando os arquivos já preparados no staging.
- Atualiza imediatamente os endereços de acesso exibidos na tela.
- Exibe um aviso claro informando que conexões de clientes no endereço anterior foram interrompidas.

### Interface Preferida vs. Interface da Sessão Atual
Na aba **`[4] Rede`**, você pode pressionar **`[f]`** para salvar uma interface como **Interface Preferida (Padrão)** no arquivo `config/labdrop.toml`:
- **Interface Preferida:** fica salva entre execuções do aplicativo e é selecionada automaticamente quando estiver disponível.
- **Interface da Sessão Atual:** pode ser alterada livremente a qualquer momento na tela de compartilhamento sem sobrescrever sua preferência padrão.
- Se a sua interface preferida estiver desconectada ao abrir o LabDrop, o aplicativo seleciona uma interface ativa disponível e exibe um aviso explicando o que aconteceu.

---

## 7. Como Acessar os Arquivos nos Computadores de Destino

Na aba **`[5] Compartilhamento`**, o painel principal mostra de forma limpa os **Endereços Principais de Acesso** (pressione **`[d]`** se quiser expandir todas as instruções passo a passo, URLs secundárias e logs):

| Sistema de Destino | Como Acessar |
| :--- | :--- |
| **Windows 10 / 11 (Descoberta Automática)** | Abra o **Explorador de Arquivos -> Rede**. O computador aparecerá automaticamente em *Computadores* ou *Outros Dispositivos* como **`LabDrop (<NomeDoPC>)`**. |
| **Windows 10 / 11 (Acesso Direto no Explorer)** | Cole na barra de endereços do Explorador de Arquivos o caminho exibido na tela: `\\<IP>\LabDrop` (SMB) ou `\\<IP>@8080\DavWWWRoot\LabDrop` (WebDAV). |
| **Linux (Fedora, Ubuntu, Linux Mint)** | Abra o gerenciador **Arquivos (Nautilus / Nemo / Dolphin) -> Outros Locais / Rede** e clique em **LabDrop**, ou conecte-se ao servidor `smb://<IP>/LabDrop` ou `dav://<IP>:8080/LabDrop/`. |
| **Qualquer Navegador Web (Windows / Linux)** | Acesse `http://<IP>:8080/` para baixar arquivos individuais ou clicar em **Download All (.ZIP)** para baixar tudo em um único pacote `.ZIP`. |

---

## 8. Samba no Linux: Quando é Necessário e Como Instalar

O LabDrop usa uma arquitetura em camadas (**SMB + WebDAV RFC 4918 + HTTP + Descoberta SSDP/WSD/mDNS**):

1. **No Windows:** utiliza o servidor SMB nativo do próprio sistema (`LanmanServer`), além de WebDAV e HTTP.
2. **No Linux:**
   - **Mesmo sem o Samba instalado**, o LabDrop funciona imediatamente através de **WebDAV nativo + HTTP + Descoberta na Rede**.
   - Se o **Samba** estiver instalado no Linux transmissor, o LabDrop também ativa o compartilhamento SMB:
     - **Sem root (Porta 4445):** isolado em user namespace, sem pedir senha de administrador.
     - **Com `sudo` (Porta 445 padrão):** compatibilidade total com descoberta SMB nativa do Windows Explorer (pressione **`[e]`** na aba `[4] Rede` para reiniciar com `sudo` na porta 445).

### Como verificar e instalar o Samba no Linux
As abas **`[4] Rede`** e **`[6] Diagnóstico`** mostram claramente se o Samba está instalado e disponível. Caso não esteja instalado e você queira utilizá-lo:
- Pressione **`[i]`** na aba `[4] Rede` ou `[6] Diagnóstico` para instalar pelo gerenciador de pacotes do sistema, ou execute manualmente:
  - **Fedora:** `sudo dnf install samba`
  - **Ubuntu / Linux Mint:** `sudo apt update && sudo apt install samba`

---

## 9. Solução de Problemas Comuns (Troubleshooting)

- **`bash: ./labdrop-bootstrapper: Permission denied` no Linux:**
  O navegador baixa arquivos sem o atributo executável (`+x`). Rode `chmod +x ~/Downloads/labdrop-bootstrapper` e execute-o novamente **sem `sudo`**. Ou baixe o pacote `labdrop-bootstrapper-linux-x86_64.tar.gz`, que já vem com permissão de execução.
- **`sudo: /home/.../labdrop-bootstrapper: command not found`:**
  O comando `sudo` retorna `command not found` quando o arquivo apontado não tem permissão `+x`. Não use `sudo` no bootstrapper; use apenas `chmod +x` e execute com seu usuário normal.
- **Pendrive montado com `noexec` no Linux:**
  Algumas distribuições montam pendrives FAT32/NTFS com a trava `noexec`. Nesse caso, instale o LabDrop localmente no PC (`~/LabDrop`) pelo bootstrapper ou copie o binário para `/var/tmp`:
  ```bash
  cp /caminho/do/pendrive/LabDrop/labdrop /var/tmp/labdrop && chmod +x /var/tmp/labdrop && /var/tmp/labdrop
  ```
- **O computador não aparece automaticamente na aba "Rede" do Windows:**
  Verifique se o perfil de rede do Windows está definido como **Rede Privada** (redes marcadas como *Públicas* no Windows bloqueiam a descoberta automática WS-Discovery/SSDP) ou se o Wi-Fi da instituição possui isolamento de clientes (AP Isolation). Nesses casos, cole o endereço direto exibido na tela (`\\<IP>@8080\DavWWWRoot\LabDrop` ou `http://<IP>:8080/`).

---

## 10. Segurança e Limpeza Automática

- **Somente-Leitura e Temporário:** Todos os protocolos (SMB, WebDAV e HTTP) expõem apenas a pasta temporária de staging em modo **somente-leitura (read-only)** e bloqueiam qualquer tentativa de navegação fora dela (`..`).
- **Acesso Sem Senha na Rede Selecionada:** Durante uma sessão ativa, qualquer computador conectado à mesma sub-rede da interface selecionada pode ler os arquivos compartilhados. Não compartilhe documentos confidenciais em redes públicas.
- **Limpeza Garantida ao Sair ou Fechar o Terminal:** Ao parar a sessão (`[s]`) ou fechar a janela do terminal, o LabDrop encerra todos os processos filhos (`smbd`, `smbd-notifyd`, `smbd-cleanupd`, WebDAV/HTTP), remove as regras temporárias de firewall e apaga os arquivos temporários de staging — garantindo que o pendrive nunca fique preso ao tentar ejetar.

---

## 11. Para Desenvolvedores (Compilação a partir do Código-Fonte)

Se você deseja modificar o código-fonte do LabDrop ou compilar os binários manualmente:

### Pré-requisitos de Compilação
- Rust (`stable`) com os alvos `x86_64-unknown-linux-musl` e `x86_64-pc-windows-gnu` (ou `x86_64-pc-windows-msvc` no Windows).
- No Fedora Linux: `sudo dnf install mingw64-gcc samba samba-client`
- No Ubuntu / Mint: `sudo apt install musl-tools gcc-mingw-w64-x86-64 samba smbclient`

### Comandos Principais
```bash
# Executar a suíte de testes unitários e de integração
cargo test --workspace

# Construir todos os binários de release (Linux estático MUSL + Windows PE x86-64 + .tar.gz + SHA256SUMS.txt) em ./dist/
./scripts/build-release.sh

# Executar a suíte completa de verificação multiplataforma e recuperação de falhas
./scripts/run-cross-tests.sh
```
