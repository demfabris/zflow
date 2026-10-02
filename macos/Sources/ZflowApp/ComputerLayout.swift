import SwiftUI

/// One computer on the arrangement canvas. The canvas draws only placed
/// tiles, at the spot the layout gives them.
struct LayoutTile: Identifiable, Equatable {
  var computer: Computer
  /// A paired computer's state; nil for this Mac.
  var state: Peer.State?
  var placed = true
  var id: String { computer.id }

  /// The layout's computers, each paired one with its state.
  static func tiles(_ layout: Layout, peers: [Peer]) -> [LayoutTile] {
    layout.monitors.map { computer in
      let peer = computer.peer.flatMap { name in peers.first { $0.name == name } }
      return LayoutTile(computer: computer, state: peer?.state)
    }
  }
}

struct ComputerLayout: View {
  var tiles: [LayoutTile]
  var move: (Computer, Int, Int, Int) -> Void
  @State private var dragging: String?
  @State private var translation = CGSize.zero

  private var placed: [LayoutTile] { tiles.filter(\.placed) }

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
        RoundedRectangle(cornerRadius: 16).fill(.quinary)
        dots
        if placed.isEmpty { Text("Detecting this Mac’s displays…").foregroundStyle(.secondary) }
        ForEach(placed) { tile in
          accessibleTile(tile, scale: scale, origin: origin)
        }
      }
      .clipShape(.rect(cornerRadius: 16))
    }
  }

  /// A dotted backdrop, like a desk mat.
  private var dots: some View {
    Canvas { context, size in
      let spacing: CGFloat = 18
      var path = Path()
      for x in stride(from: spacing / 2, to: size.width, by: spacing) {
        for y in stride(from: spacing / 2, to: size.height, by: spacing) {
          path.addEllipse(in: CGRect(x: x - 0.75, y: y - 0.75, width: 1.5, height: 1.5))
        }
      }
      context.fill(path, with: .color(.secondary.opacity(0.3)))
    }
    .accessibilityHidden(true)
  }

  private func positionedTile(_ tile: LayoutTile, scale: CGFloat, origin: CGPoint) -> some View {
    let computer = tile.computer
    let active: Bool = dragging == computer.id
    let offset: CGSize = active ? translation : .zero
    let midpointX: CGFloat = CGFloat(computer.x) + CGFloat(computer.width) / 2
    let midpointY: CGFloat = CGFloat(computer.y) + CGFloat(computer.height) / 2
    let center = CGPoint(
      x: origin.x + midpointX * scale + offset.width,
      y: origin.y + midpointY * scale + offset.height)
    return tileView(tile, scale: scale, active: active)
      .position(center)
      .zIndex(active ? 1 : 0)
  }

  private func accessibleTile(_ tile: LayoutTile, scale: CGFloat, origin: CGPoint) -> some View {
    let computer = tile.computer
    return positionedTile(tile, scale: scale, origin: origin)
      .gesture(dragGesture(for: computer, scale: scale))
      .accessibilityElement(children: .ignore)
      .accessibilityLabel(Text(computer.label))
      .accessibilityValue(
        Text("\(tile.state?.label ?? "This Mac"), position \(computer.x), \(computer.y)")
      )
      .accessibilityAction(named: Text("Move left")) {
        move(computer, computer.x - 100, computer.y, 150)
      }
      .accessibilityAction(named: Text("Move right")) {
        move(computer, computer.x + 100, computer.y, 150)
      }
      .accessibilityAction(named: Text("Move up")) {
        move(computer, computer.x, computer.y - 100, 150)
      }
      .accessibilityAction(named: Text("Move down")) {
        move(computer, computer.x, computer.y + 100, 150)
      }
  }

  private func dragGesture(for computer: Computer, scale: CGFloat) -> some Gesture {
    DragGesture(minimumDistance: 2)
      .onChanged { (value: DragGesture.Value) in
        dragging = computer.id
        translation = value.translation
      }
      .onEnded { (value: DragGesture.Value) in
        let deltaX: Int = Int((value.translation.width / scale).rounded())
        let deltaY: Int = Int((value.translation.height / scale).rounded())
        let tolerance: Int = Int((CGFloat(14) / scale).rounded())
        move(computer, computer.x + deltaX, computer.y + deltaY, tolerance)
        dragging = nil
        translation = .zero
      }
  }

  /// This Mac has an accent outline. A paired computer that is not
  /// connected is drawn faded, with a dashed outline.
  private func tileView(_ tile: LayoutTile, scale: CGFloat, active: Bool) -> some View {
    let computer = tile.computer
    let local = computer.peer == nil
    let away = !local && [nil, .paired, .unreachable].contains(tile.state)
    let shape = RoundedRectangle(cornerRadius: 10)
    return VStack(alignment: .leading, spacing: 2) {
      Image(systemName: local ? "laptopcomputer" : "desktopcomputer")
        .font(.title3)
        .foregroundStyle(local ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
      Spacer(minLength: 4)
      Text(computer.label).font(.callout.weight(.semibold)).lineLimit(1)
      if let state = tile.state {
        HStack(spacing: 5) {
          StateDot(state: state)
          Text(state.label).lineLimit(1)
        }
        .font(.caption).foregroundStyle(.secondary)
      } else if local && computer.label != "This Mac" {
        Text("This Mac").font(.caption).foregroundStyle(.secondary)
      }
    }
    .padding(10)
    .frame(
      width: max(110, CGFloat(computer.width) * scale),
      height: max(76, CGFloat(computer.height) * scale),
      alignment: .topLeading
    )
    .background(Color(nsColor: .controlBackgroundColor).opacity(away ? 0.55 : 1), in: shape)
    .overlay {
      if active || local {
        shape.strokeBorder(Color.accentColor, lineWidth: 2)
      } else if away {
        shape.strokeBorder(
          Color.secondary.opacity(0.5), style: StrokeStyle(lineWidth: 1.5, dash: [5, 4]))
      } else {
        shape.strokeBorder(Color.secondary.opacity(0.35), lineWidth: 1)
      }
    }
    .shadow(
      color: .black.opacity(active ? 0.13 : 0.05), radius: active ? 8 : 3, y: active ? 4 : 1
    )
  }

  private var layoutBounds: CGRect {
    let bounds = placed.reduce(CGRect.null) {
      $0.union(
        CGRect(
          x: $1.computer.x, y: $1.computer.y, width: $1.computer.width, height: $1.computer.height))
    }
    return bounds.isNull ? CGRect(x: 0, y: 0, width: 1600, height: 1000) : bounds
  }
}
