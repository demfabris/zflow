import SwiftUI

struct ComputerLayout: View {
  var computers: [Computer]
  var move: (Computer, Int, Int, Int) -> Void
  var forget: (String) -> Void
  @State private var dragging: String?
  @State private var translation = CGSize.zero
  @State private var forgetting: Computer?

  var body: some View {
    GeometryReader { area in
      let bounds = layoutBounds
      let scale = min(
        (area.size.width - 90) / max(bounds.width, 1),
        (area.size.height - 70) / max(bounds.height, 1), 0.12)
      let origin = CGPoint(
        x: (area.size.width - bounds.width * scale) / 2 - bounds.minX * scale,
        y: (area.size.height - bounds.height * scale) / 2 - bounds.minY * scale)
      ZStack {
        RoundedRectangle(cornerRadius: 10).fill(.quaternary.opacity(0.35))
        if computers.isEmpty { Text("Detecting this Mac’s displays…").foregroundStyle(.secondary) }
        ForEach(computers) { computer in
          let active = dragging == computer.id
          VStack(spacing: 6) {
            Image(systemName: computer.peer == nil ? "laptopcomputer" : "display").font(.title2)
            Text(computer.label).font(.callout.weight(.medium)).lineLimit(1)
          }
          .frame(
            width: max(70, Double(computer.width) * scale),
            height: max(60, Double(computer.height) * scale)
          )
          .background(
            computer.peer == nil
              ? Color.accentColor.opacity(0.12) : Color(nsColor: .controlBackgroundColor),
            in: RoundedRectangle(cornerRadius: 8)
          )
          .overlay {
            RoundedRectangle(cornerRadius: 8).strokeBorder(
              active ? Color.accentColor : Color.secondary.opacity(0.35), lineWidth: active ? 2 : 1)
          }
          .shadow(
            color: .black.opacity(active ? 0.13 : 0.04), radius: active ? 8 : 2, y: active ? 4 : 1
          )
          .position(
            x: origin.x + (Double(computer.x) + Double(computer.width) / 2) * scale
              + (active ? translation.width : 0),
            y: origin.y + (Double(computer.y) + Double(computer.height) / 2) * scale
              + (active ? translation.height : 0)
          )
          .zIndex(active ? 1 : 0)
          .gesture(
            DragGesture(minimumDistance: 2)
              .onChanged { value in
                dragging = computer.id
                translation = value.translation
              }
              .onEnded { value in
                move(
                  computer, computer.x + Int((value.translation.width / scale).rounded()),
                  computer.y + Int((value.translation.height / scale).rounded()),
                  Int((14 / scale).rounded()))
                dragging = nil
                translation = .zero
              }
          )
          .contextMenu {
            if computer.peer != nil {
              Button("Forget Computer…", role: .destructive) { forgetting = computer }
            }
          }
          .accessibilityElement(children: .ignore)
          .accessibilityLabel(computer.label)
          .accessibilityValue("Position \(computer.x), \(computer.y)")
          .accessibilityAction(named: "Move left") {
            move(computer, computer.x - 100, computer.y, 150)
          }
          .accessibilityAction(named: "Move right") {
            move(computer, computer.x + 100, computer.y, 150)
          }
          .accessibilityAction(named: "Move up") {
            move(computer, computer.x, computer.y - 100, 150)
          }
          .accessibilityAction(named: "Move down") {
            move(computer, computer.x, computer.y + 100, 150)
          }
        }
      }.clipped()
    }
    .confirmationDialog(
      "Forget \(forgetting?.label ?? "computer")?",
      isPresented: Binding(get: { forgetting != nil }, set: { if !$0 { forgetting = nil } }),
      titleVisibility: .visible
    ) {
      Button("Forget Computer", role: .destructive) {
        if let name = forgetting?.peer { forget(name) }
        forgetting = nil
      }
      Button("Cancel", role: .cancel) { forgetting = nil }
    } message: {
      Text("You will need to pair again to share input with this computer.")
    }
  }
  private var layoutBounds: CGRect {
    computers.reduce(CGRect.null) {
      $0.union(CGRect(x: $1.x, y: $1.y, width: $1.width, height: $1.height))
    }.isNull
      ? CGRect(x: 0, y: 0, width: 1600, height: 1000)
      : computers.reduce(CGRect.null) {
        $0.union(CGRect(x: $1.x, y: $1.y, width: $1.width, height: $1.height))
      }
  }
}
