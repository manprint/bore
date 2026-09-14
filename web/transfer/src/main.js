import "./styles.css";

// Inert bootstrap (Phase 0 scaffold). No network, no storage, no transfer:
// later phases attach the control WebSocket, catalog and download paths here.
const status = document.getElementById("room-status");
if (status !== null && status.textContent === "") {
  status.textContent = "Connessione alla room…";
}
